use nodejs_resolver::Resolver;
use std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Instant};
use tokio::sync::mpsc::error::TryRecvError;
use tracing::info;

use crate::{
    analyze_deps::{add_swc_helper_deps, analyze_deps},
    ast::build_js_ast,
    compiler::{Compiler, Context},
    config::Config,
    load::load,
    module::{Dependency, Module, ModuleAst, ModuleId, ModuleInfo},
    parse::parse,
    resolve::{get_resolver, resolve},
    transform::transform,
};

#[derive(Debug)]
struct Task {
    path: String,
    is_entry: bool,
}

impl Compiler {
    pub fn build(&self) {
        info!("build");
        let t_build = Instant::now();
        self.build_module_graph();
        let t_build = t_build.elapsed();
        // build chunk map 应该放 generate 阶段
        // 和 chunk 相关的都属于 generate

        info!("build done in {}ms", t_build.as_millis());
    }

    // TODO:
    // - 处理出错（比如找不到模块）的情况，现在会直接挂起
    fn build_module_graph(&self) {
        info!("build module graph");

        let entries =
            get_entries(&self.context.root, &self.context.config).expect("entry not found");
        if entries.is_empty() {
            panic!("entry not found");
        }

        let resolver = Arc::new(get_resolver(Some(
            self.context.config.resolve.alias.clone(),
        )));
        let mut queue: VecDeque<Task> = VecDeque::new();
        for entry in entries {
            queue.push_back(Task {
                path: entry.to_str().unwrap().to_string(),
                is_entry: true,
            });
        }

        let (rs, mut rr) = tokio::sync::mpsc::unbounded_channel::<(
            Module,
            Vec<(String, Option<String>, Dependency)>,
            Task,
        )>();
        let mut active_task_count: usize = 0;
        let mut t_main_thread: usize = 0;
        let mut module_count: usize = 0;
        tokio::task::block_in_place(move || loop {
            let mut module_graph = self.context.module_graph.write().unwrap();
            while let Some(task) = queue.pop_front() {
                let resolver = resolver.clone();
                let context = self.context.clone();
                tokio::spawn({
                    active_task_count += 1;
                    module_count += 1;
                    let rs = rs.clone();
                    async move {
                        let (module, dependencies, task) =
                            Compiler::build_module(context, task, resolver);
                        rs.send((module, dependencies, task))
                            .expect("send task failed");
                    }
                });
            }
            match rr.try_recv() {
                Ok((module, deps, task)) => {
                    let t = Instant::now();

                    // current module
                    let module_id = module.id.clone();
                    // 只有处理 entry 时，module 会不存在于 module_graph 里
                    // 否则，module 会存在于 module_graph 里，只需要补充 info 信息即可
                    if task.is_entry {
                        module_graph.add_module(module);
                    } else {
                        let m = module_graph.get_module_mut(&module_id).unwrap();
                        m.add_info(module.info);
                    }

                    // deps
                    deps.iter().for_each(|dep| {
                        let resolved_path = dep.0.clone();
                        let is_external = dep.1.is_some();
                        let dep_module_id = ModuleId::new(resolved_path.clone());
                        let dependency = dep.2.clone();

                        if !module_graph.has_module(&dep_module_id) {
                            let module = if is_external {
                                let external = dep.1.as_ref().unwrap();
                                let code = format!("module.exports = {};", external);
                                let ast = build_js_ast(
                                    format!("external_{}", &resolved_path).as_str(),
                                    code.as_str(),
                                    &self.context,
                                );
                                Module::new(
                                    dep_module_id.clone(),
                                    false,
                                    Some(ModuleInfo {
                                        ast: ModuleAst::Script(ast),
                                        path: resolved_path,
                                        external: Some(external.to_string()),
                                    }),
                                )
                            } else {
                                queue.push_back(Task {
                                    path: resolved_path,
                                    is_entry: false,
                                });
                                Module::new(dep_module_id.clone(), false, None)
                            };
                            // 拿到依赖之后需要直接添加 module 到 module_graph 里，不能等依赖 build 完再添加
                            // 由于是异步处理各个模块，后者会导致大量重复任务的 build_module 任务（3 倍左右）
                            module_graph.add_module(module);
                        }
                        module_graph.add_dependency(&module_id, &dep_module_id, dependency);
                    });
                    active_task_count -= 1;
                    let t = t.elapsed();
                    t_main_thread += t.as_micros() as usize;
                }
                Err(TryRecvError::Empty) => {
                    if active_task_count == 0 {
                        info!("build time in main thread: {}ms", t_main_thread / 1000);
                        info!("module count: {}", module_count);
                        break;
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    break;
                }
            }
        });
    }

    fn build_module(
        context: Arc<Context>,
        task: Task,
        resolver: Arc<Resolver>,
    ) -> (Module, Vec<(String, Option<String>, Dependency)>, Task) {
        let module_id = ModuleId::new(task.path.clone());

        // load
        let content = load(&task.path, &context);

        // parse
        let mut ast = parse(&content, &task.path, &context);

        // analyze deps
        // transform 之后的 helper 怎么处理？比如 @swc/helpers/_/_interop_require_default
        // 解法是在 transform 之后补一遍以 @swc/helpers 开头的 require 方法
        let mut deps = analyze_deps(&ast);

        // transform
        transform(&mut ast, &context);

        // add @swc/helpers deps
        add_swc_helper_deps(&mut deps, &ast);

        // resolve
        let dependencies: Vec<(String, Option<String>, Dependency)> = deps
            .iter()
            .map(|dep| {
                let (x, y) = resolve(&task.path, dep, &resolver, &context);
                (x, y, dep.clone())
            })
            .collect();

        let info = ModuleInfo {
            ast,
            path: task.path.clone(),
            external: None,
        };
        let module = Module::new(module_id, task.is_entry, Some(info));

        (module, dependencies, task)
    }
}

fn get_entries(root: &PathBuf, config: &Config) -> Option<Vec<std::path::PathBuf>> {
    let entry = &config.entry;
    if entry.is_empty() {
        let file_paths = vec!["src/index.tsx", "src/index.ts", "index.tsx", "index.ts"];
        for file_path in file_paths {
            let file_path = root.join(file_path);
            if file_path.exists() {
                return Some(vec![file_path]);
            }
        }
    } else {
        let vals = entry
            .values()
            .map(|v| root.join(v))
            .collect::<Vec<std::path::PathBuf>>();
        return Some(vals);
    }
    None
}

#[cfg(test)]
mod tests {
    use petgraph::prelude::EdgeRef;
    use petgraph::visit::IntoEdgeReferences;

    use crate::{compiler, config};

    #[tokio::test(flavor = "multi_thread")]
    async fn test_build() {
        let (module_ids, references) = build("test/build/normal");
        // let (module_ids, _) = build("examples/normal");
        assert_eq!(
            module_ids.join(","),
            "bar_1.ts,bar_2.ts,foo.ts,index.ts".to_string()
        );
        assert_eq!(
            references
                .into_iter()
                .map(|(source, target)| { format!("{} -> {}", source, target) })
                .collect::<Vec<String>>()
                .join(","),
            "bar_1.ts -> foo.ts,bar_2.ts -> foo.ts,index.ts -> bar_1.ts,index.ts -> bar_2.ts"
                .to_string()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_build_css() {
        let (module_ids, references) = build("test/build/css");
        assert_eq!(
            module_ids.join(","),
            "foo.css,index.css,index.ts,umi-logo.png".to_string()
        );
        assert_eq!(
            references
                .into_iter()
                .map(|(source, target)| { format!("{} -> {}", source, target) })
                .collect::<Vec<String>>()
                .join(","),
            "index.css -> foo.css,index.css -> umi-logo.png,index.ts -> index.css".to_string()
        );
    }

    fn build(base: &str) -> (Vec<String>, Vec<(String, String)>) {
        let current_dir = std::env::current_dir().unwrap();
        // let fixtures = current_dir.join("test/build");
        let pnpm_dir = current_dir.join("node_modules/.pnpm");
        let root = current_dir.join(base);
        let config = config::Config::new(&root).unwrap();
        let compiler = compiler::Compiler::new(config, root.clone());
        compiler.build();
        let module_graph = compiler.context.module_graph.read().unwrap();
        let mut module_ids: Vec<String> = module_graph
            .graph
            .node_weights()
            .into_iter()
            .map(|module| {
                module
                    .id
                    .id
                    .to_string()
                    .replace(format!("{}/", root.to_str().unwrap()).as_str(), "")
                    .replace(pnpm_dir.to_str().unwrap(), "")
            })
            .collect();
        module_ids.sort_by_key(|module_id| module_id.to_string());
        let mut references: Vec<(String, String)> = module_graph
            .graph
            .edge_references()
            .into_iter()
            .map(|edge| {
                let source = &module_graph.graph[edge.source()].id.id;
                let target = &module_graph.graph[edge.target()].id.id;
                (
                    source
                        .to_string()
                        .replace(format!("{}/", root.to_str().unwrap()).as_str(), "")
                        .replace(pnpm_dir.to_str().unwrap(), ""),
                    target
                        .to_string()
                        .replace(format!("{}/", root.to_str().unwrap()).as_str(), "")
                        .replace(pnpm_dir.to_str().unwrap(), ""),
                )
            })
            .collect();
        references.sort_by_key(|(source, target)| format!("{} -> {}", source, target));

        println!("module_ids:");
        for module_id in &module_ids {
            println!("  - {:?}", module_id);
        }
        println!("references:");
        for (source, target) in &references {
            println!("  - {} -> {}", source, target);
        }

        (module_ids, references)
    }
}