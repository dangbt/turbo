use anyhow::Result;
use turbo_tasks::Vc;
use turbo_tasks_fs::{FileSystem, FileSystemPath};
use turbopack_core::resolve::{
    find_context_file,
    options::{
        ConditionValue, ImportMap, ImportMapping, ResolutionConditions, ResolveInPackage,
        ResolveIntoPackage, ResolveModules, ResolveOptions,
    },
    AliasMap, AliasPattern, FindContextFileResult,
};
use turbopack_ecmascript::typescript::resolve::{
    apply_tsconfig_resolve_options, tsconfig, tsconfig_resolve_options,
};

use crate::resolve_options_context::ResolveOptionsContext;

const NODE_EXTERNALS: [&str; 51] = [
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "sys",
    "timers",
    "timers/promises",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
    "pnpapi",
];

const EDGE_NODE_EXTERNALS: [&str; 5] = ["buffer", "events", "assert", "util", "async_hooks"];

#[turbo_tasks::function]
async fn base_resolve_options(
    resolve_path: Vc<FileSystemPath>,
    options_context: Vc<ResolveOptionsContext>,
) -> Result<Vc<ResolveOptions>> {
    let parent = resolve_path.parent().resolve().await?;
    if parent != resolve_path {
        return Ok(base_resolve_options(parent, options_context));
    }
    let resolve_path_value = resolve_path.await?;
    let opt = options_context.await?;
    let emulating = opt.emulate_environment;
    let root = resolve_path_value.fs.root();
    let mut direct_mappings = AliasMap::new();
    let node_externals = if let Some(environment) = emulating {
        environment.node_externals().await?.clone_value()
    } else {
        opt.enable_node_externals
    };
    if node_externals {
        for req in NODE_EXTERNALS {
            direct_mappings.insert(
                AliasPattern::exact(req),
                ImportMapping::External(None).into(),
            );
            direct_mappings.insert(
                AliasPattern::exact(format!("node:{req}")),
                ImportMapping::External(None).into(),
            );
        }
    }
    if opt.enable_edge_node_externals {
        for req in EDGE_NODE_EXTERNALS {
            direct_mappings.insert(
                AliasPattern::exact(req),
                ImportMapping::External(Some(format!("node:{req}"))).into(),
            );
            direct_mappings.insert(
                AliasPattern::exact(format!("node:{req}")),
                ImportMapping::External(None).into(),
            );
        }
    }

    let mut import_map = ImportMap::new(direct_mappings);
    if let Some(additional_import_map) = opt.import_map {
        let additional_import_map = additional_import_map.await?;
        import_map.extend_ref(&additional_import_map);
    }
    let import_map = import_map.cell();

    let plugins = opt.plugins.clone();

    let conditions = {
        let mut conditions: ResolutionConditions = [
            ("import".to_string(), ConditionValue::Unknown),
            ("require".to_string(), ConditionValue::Unknown),
        ]
        .into_iter()
        .collect();
        if opt.browser {
            conditions.insert("browser".to_string(), ConditionValue::Set);
        }
        if opt.module {
            conditions.insert("module".to_string(), ConditionValue::Set);
        }
        if let Some(environment) = emulating {
            for condition in environment.resolve_conditions().await?.iter() {
                conditions.insert(condition.to_string(), ConditionValue::Set);
            }
        }
        for condition in opt.custom_conditions.iter() {
            conditions.insert(condition.to_string(), ConditionValue::Set);
        }
        // Infer some well-known conditions
        let dev = conditions.get("development").cloned();
        let prod = conditions.get("production").cloned();
        if prod.is_none() {
            conditions.insert(
                "production".to_string(),
                if matches!(dev, Some(ConditionValue::Set)) {
                    ConditionValue::Unset
                } else {
                    ConditionValue::Unknown
                },
            );
        }
        if dev.is_none() {
            conditions.insert(
                "development".to_string(),
                if matches!(prod, Some(ConditionValue::Set)) {
                    ConditionValue::Unset
                } else {
                    ConditionValue::Unknown
                },
            );
        }
        conditions
    };

    let extensions = if let Some(custom_extension) = &opt.custom_extensions {
        custom_extension.clone()
    } else if let Some(environment) = emulating {
        environment.resolve_extensions().await?.clone_value()
    } else {
        let mut ext = Vec::new();
        if opt.enable_typescript && opt.enable_react {
            ext.push(".tsx".to_string());
        }
        if opt.enable_typescript {
            ext.push(".ts".to_string());
        }
        if opt.enable_react {
            ext.push(".jsx".to_string());
        }
        ext.push(".js".to_string());
        if opt.enable_mjs_extension {
            ext.push(".mjs".to_string());
        }
        if opt.enable_node_native_modules {
            ext.push(".node".to_string());
        }
        ext.push(".json".to_string());
        ext
    };
    Ok(ResolveOptions {
        extensions,
        modules: if let Some(environment) = emulating {
            if *environment.resolve_node_modules().await? {
                vec![ResolveModules::Nested(
                    root,
                    vec!["node_modules".to_string()],
                )]
            } else {
                Vec::new()
            }
        } else {
            let mut mods = Vec::new();
            if let Some(dir) = opt.enable_node_modules {
                mods.push(ResolveModules::Nested(
                    dir,
                    vec!["node_modules".to_string()],
                ));
            }
            mods
        },
        into_package: {
            let mut resolve_into = vec![ResolveIntoPackage::ExportsField {
                conditions: conditions.clone(),
                unspecified_conditions: ConditionValue::Unset,
            }];
            if opt.browser {
                resolve_into.push(ResolveIntoPackage::MainField {
                    field: "browser".to_string(),
                    extensions: None,
                });
            }
            if opt.module {
                resolve_into.push(ResolveIntoPackage::MainField {
                    field: "module".to_string(),
                    extensions: None,
                });
            }
            resolve_into.push(ResolveIntoPackage::MainField {
                field: "main".to_string(),
                extensions: None,
            });
            resolve_into.push(ResolveIntoPackage::Default("index".to_string()));
            resolve_into
        },
        in_package: {
            let mut resolve_in = vec![ResolveInPackage::ImportsField {
                conditions,
                unspecified_conditions: ConditionValue::Unset,
            }];
            if opt.browser {
                resolve_in.push(ResolveInPackage::AliasField("browser".to_string()));
            }
            resolve_in
        },
        import_map: Some(import_map),
        resolved_map: opt.resolved_map,
        plugins,
        ..Default::default()
    }
    .into())
}

#[turbo_tasks::function]
pub async fn resolve_options(
    resolve_path: Vc<FileSystemPath>,
    options_context: Vc<ResolveOptionsContext>,
) -> Result<Vc<ResolveOptions>> {
    let options_context_value = options_context.await?;
    if !options_context_value.rules.is_empty() {
        let context_value = &*resolve_path.await?;
        for (condition, new_options_context) in options_context_value.rules.iter() {
            if condition.matches(context_value).await? {
                return Ok(resolve_options(resolve_path, *new_options_context));
            }
        }
    }

    let resolve_options = base_resolve_options(resolve_path, options_context);

    let resolve_options = if options_context_value.enable_typescript {
        let tsconfig = find_context_file(resolve_path, tsconfig()).await?;
        match *tsconfig {
            FindContextFileResult::Found(path, _) => {
                apply_tsconfig_resolve_options(resolve_options, tsconfig_resolve_options(path))
            }
            FindContextFileResult::NotFound(_) => resolve_options,
        }
    } else {
        resolve_options
    };

    // Make sure to always apply `options_context.import_map` last, so it properly
    // overwrites any other mappings.
    let resolve_options = options_context_value
        .import_map
        .map(|import_map| resolve_options.with_extended_import_map(import_map))
        .unwrap_or(resolve_options);
    // And the same for the fallback_import_map
    let resolve_options = options_context_value
        .fallback_import_map
        .map(|fallback_import_map| {
            resolve_options.with_extended_fallback_import_map(fallback_import_map)
        })
        .unwrap_or(resolve_options);

    Ok(resolve_options)
}
