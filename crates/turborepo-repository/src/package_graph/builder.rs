use std::{
    backtrace::Backtrace,
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
};

use petgraph::graph::{Graph, NodeIndex};
use tracing::{warn, Instrument};
use turbopath::{
    AbsoluteSystemPath, AbsoluteSystemPathBuf, AnchoredSystemPath, AnchoredSystemPathBuf,
    RelativeUnixPathBuf,
};
use turborepo_graph_utils as graph;
use turborepo_lockfiles::Lockfile;

use super::{PackageGraph, PackageInfo, PackageName, PackageNode};
use crate::{
    discovery::{
        self, CachingPackageDiscovery, LocalPackageDiscoveryBuilder, PackageDiscovery,
        PackageDiscoveryBuilder,
    },
    package_json::PackageJson,
};

pub struct PackageGraphBuilder<'a, T> {
    repo_root: &'a AbsoluteSystemPath,
    root_package_json: PackageJson,
    is_single_package: bool,
    package_jsons: Option<HashMap<AbsoluteSystemPathBuf, PackageJson>>,
    lockfile: Option<Box<dyn Lockfile>>,
    package_discovery: T,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not resolve workspaces: {0}")]
    PackageManager(
        #[from] crate::package_manager::Error,
        #[backtrace] Backtrace,
    ),
    #[error(
        "Failed to add workspace \"{name}\" from \"{path}\", it already exists at \
         \"{existing_path}\""
    )]
    DuplicateWorkspace {
        name: String,
        path: String,
        existing_path: String,
    },
    #[error("path error: {0}")]
    Path(#[from] turbopath::PathError),
    #[error("unable to parse workspace package.json: {0}")]
    PackageJson(#[from] crate::package_json::Error),
    #[error("package.json must have a name field:\n{0}")]
    PackageJsonMissingName(AbsoluteSystemPathBuf),
    #[error("Invalid package dependency graph: {0}")]
    InvalidPackageGraph(#[source] graph::Error),
    #[error(transparent)]
    Lockfile(#[from] turborepo_lockfiles::Error),
    #[error(transparent)]
    Discovery(#[from] crate::discovery::Error),
}

impl<'a> PackageGraphBuilder<'a, LocalPackageDiscoveryBuilder> {
    pub fn new(repo_root: &'a AbsoluteSystemPath, root_package_json: PackageJson) -> Self {
        Self {
            package_discovery: LocalPackageDiscoveryBuilder::new(
                repo_root.to_owned(),
                None,
                Some(root_package_json.clone()),
            ),
            repo_root,
            root_package_json,
            is_single_package: false,
            package_jsons: None,
            lockfile: None,
        }
    }
}

impl<'a, P> PackageGraphBuilder<'a, P> {
    pub fn with_single_package_mode(mut self, is_single: bool) -> Self {
        self.is_single_package = is_single;
        self
    }

    #[allow(dead_code)]
    pub fn with_package_jsons(
        mut self,
        package_jsons: Option<HashMap<AbsoluteSystemPathBuf, PackageJson>>,
    ) -> Self {
        self.package_jsons = package_jsons;
        self
    }

    #[allow(dead_code)]
    pub fn with_lockfile(mut self, lockfile: Option<Box<dyn Lockfile>>) -> Self {
        self.lockfile = lockfile;
        self
    }

    /// Set the package discovery strategy to use. Note that whatever strategy
    /// selected here will be wrapped in a `CachingPackageDiscovery` to
    /// prevent unnecessary work during building.
    pub fn with_package_discovery<P2: PackageDiscoveryBuilder>(
        self,
        discovery: P2,
    ) -> PackageGraphBuilder<'a, P2> {
        PackageGraphBuilder {
            repo_root: self.repo_root,
            root_package_json: self.root_package_json,
            is_single_package: self.is_single_package,
            package_jsons: self.package_jsons,
            lockfile: self.lockfile,
            package_discovery: discovery,
        }
    }
}

impl<'a, T> PackageGraphBuilder<'a, T>
where
    T: PackageDiscoveryBuilder,
    T::Output: Send,
    T::Error: Into<crate::package_manager::Error>,
{
    /// Build the `PackageGraph`.
    #[tracing::instrument(skip(self))]
    pub async fn build(self) -> Result<PackageGraph, Error> {
        let is_single_package = self.is_single_package;
        let state = BuildState::new(self)?;

        match is_single_package {
            true => Ok(state.build_single_package_graph().await?),
            false => {
                let state = state.parse_package_jsons().await?;
                let state = state.resolve_lockfile().await?;
                Ok(state.build_inner().await?)
            }
        }
    }
}

struct BuildState<'a, S, T> {
    repo_root: &'a AbsoluteSystemPath,
    single: bool,
    workspaces: HashMap<PackageName, PackageInfo>,
    workspace_graph: Graph<PackageNode, ()>,
    node_lookup: HashMap<PackageNode, NodeIndex>,
    lockfile: Option<Box<dyn Lockfile>>,
    package_jsons: Option<HashMap<AbsoluteSystemPathBuf, PackageJson>>,
    state: std::marker::PhantomData<S>,
    package_discovery: T,
}

// Allows us to perform workspace discovery and parse package jsons
enum ResolvedPackageManager {}

// Allows us to build the workspace graph and list over external dependencies
enum ResolvedWorkspaces {}

// Allows us to collect all transitive deps
enum ResolvedLockfile {}

impl<'a, S, T> BuildState<'a, S, T> {
    fn add_node(&mut self, node: PackageNode) -> NodeIndex {
        let idx = self.workspace_graph.add_node(node.clone());
        self.node_lookup.insert(node, idx);
        idx
    }

    fn add_root_workspace(&mut self) {
        let root_index = self.add_node(PackageNode::Root);
        let root_workspace = self.add_node(PackageNode::Workspace(PackageName::Root));
        self.workspace_graph
            .add_edge(root_workspace, root_index, ());
    }
}

impl<'a, T> BuildState<'a, ResolvedPackageManager, T>
where
    T: PackageDiscoveryBuilder,
    T::Output: Send,
    T::Error: Into<crate::package_manager::Error>,
{
    fn new(
        builder: PackageGraphBuilder<'a, T>,
    ) -> Result<
        BuildState<'a, ResolvedPackageManager, CachingPackageDiscovery<T::Output>>,
        crate::package_manager::Error,
    > {
        let PackageGraphBuilder {
            repo_root,
            root_package_json,
            is_single_package: single,

            package_jsons,
            lockfile,
            package_discovery,
        } = builder;
        let mut workspaces = HashMap::new();
        workspaces.insert(
            PackageName::Root,
            PackageInfo {
                package_json: root_package_json,
                package_json_path: AnchoredSystemPathBuf::from_raw("package.json").unwrap(),
                ..Default::default()
            },
        );

        Ok(BuildState {
            repo_root,
            single,

            workspaces,
            lockfile,
            package_jsons,
            workspace_graph: Graph::new(),
            node_lookup: HashMap::new(),
            state: std::marker::PhantomData,
            package_discovery: CachingPackageDiscovery::new(
                package_discovery.build().map_err(Into::into)?,
            ),
        })
    }
}

impl<'a, T: PackageDiscovery> BuildState<'a, ResolvedPackageManager, T> {
    fn add_json(
        &mut self,
        package_json_path: AbsoluteSystemPathBuf,
        json: PackageJson,
    ) -> Result<(), Error> {
        let relative_json_path =
            AnchoredSystemPathBuf::relative_path_between(self.repo_root, &package_json_path);
        let name = PackageName::Other(
            json.name
                .clone()
                .ok_or(Error::PackageJsonMissingName(package_json_path))?,
        );
        let entry = PackageInfo {
            package_json: json,
            package_json_path: relative_json_path,
            ..Default::default()
        };
        if let Some(existing) = self.workspaces.insert(name.clone(), entry) {
            let path = self
                .workspaces
                .get(&name)
                .expect("just inserted entry to be present")
                .package_json_path
                .clone();
            return Err(Error::DuplicateWorkspace {
                name: name.to_string(),
                path: path.to_string(),
                existing_path: existing.package_json_path.to_string(),
            });
        }
        self.add_node(PackageNode::Workspace(name));
        Ok(())
    }

    // need our own type
    #[tracing::instrument(skip(self))]
    async fn parse_package_jsons(mut self) -> Result<BuildState<'a, ResolvedWorkspaces, T>, Error> {
        // The root workspace will be present
        // we either read from disk or just read the map
        self.add_root_workspace();

        let package_jsons = match self.package_jsons.take() {
            Some(jsons) => Ok(jsons),
            None => {
                let mut jsons = HashMap::new();
                for path in self.package_discovery.discover_packages().await?.workspaces {
                    let json = PackageJson::load(&path.package_json)?;
                    jsons.insert(path.package_json, json);
                }
                Ok::<_, Error>(jsons)
            }
        }?;

        for (path, json) in package_jsons {
            match self.add_json(path, json) {
                Ok(()) => {}
                Err(Error::PackageJsonMissingName(path)) => {
                    // previous implementations of turbo would silently ignore package.json files
                    // that didn't have a name field (well, actually, if two or more had the same
                    // name, it would throw a 'name clash' error, but that's a different story)
                    //
                    // let's try to match that behavior, but log a debug message
                    tracing::debug!("ignoring package.json at {} since it has no name", path);
                }
                Err(err) => return Err(err),
            }
        }

        let Self {
            repo_root,
            single,
            workspaces,
            workspace_graph,
            node_lookup,
            lockfile,
            package_discovery,
            ..
        } = self;
        Ok(BuildState {
            repo_root,
            single,
            workspaces,
            workspace_graph,
            node_lookup,
            lockfile,
            package_discovery,
            package_jsons: None,
            state: std::marker::PhantomData,
        })
    }

    async fn build_single_package_graph(mut self) -> Result<PackageGraph, discovery::Error> {
        self.add_root_workspace();
        let Self {
            single,
            workspaces,
            workspace_graph,
            node_lookup,
            lockfile,
            mut package_discovery,
            ..
        } = self;

        let package_manager = package_discovery.discover_packages().await?.package_manager;

        debug_assert!(single, "expected single package graph");
        Ok(PackageGraph {
            graph: workspace_graph,
            node_lookup,
            packages: workspaces,
            lockfile,
            package_manager,
        })
    }
}

impl<'a, T: PackageDiscovery> BuildState<'a, ResolvedWorkspaces, T> {
    #[tracing::instrument(skip(self))]
    fn connect_internal_dependencies(&mut self) -> Result<(), Error> {
        let split_deps = self
            .workspaces
            .iter()
            .map(|(name, entry)| {
                // TODO avoid clone
                (
                    name.clone(),
                    Dependencies::new(
                        self.repo_root,
                        &entry.package_json_path,
                        &self.workspaces,
                        entry.package_json.all_dependencies(),
                    ),
                )
            })
            .collect::<Vec<_>>();
        for (name, deps) in split_deps {
            let entry = self
                .workspaces
                .get_mut(&name)
                .expect("workspace present in ");
            let Dependencies { internal, external } = deps;
            let node_idx = self
                .node_lookup
                .get(&PackageNode::Workspace(name))
                .expect("unable to find workspace node index");
            if internal.is_empty() {
                let root_idx = self
                    .node_lookup
                    .get(&PackageNode::Root)
                    .expect("root node should have index");
                self.workspace_graph.add_edge(*node_idx, *root_idx, ());
            }
            for dependency in internal {
                let dependency_idx = self
                    .node_lookup
                    .get(&PackageNode::Workspace(dependency))
                    .expect("unable to find workspace node index");
                self.workspace_graph
                    .add_edge(*node_idx, *dependency_idx, ());
            }
            entry.unresolved_external_dependencies = Some(external);
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn populate_lockfile(&mut self) -> Result<Box<dyn Lockfile>, Error> {
        let package_manager = self
            .package_discovery
            .discover_packages()
            .await?
            .package_manager;

        match self.lockfile.take() {
            Some(lockfile) => Ok(lockfile),
            None => {
                let lockfile = package_manager.read_lockfile(
                    self.repo_root,
                    self.workspaces
                        .get(&PackageName::Root)
                        .as_ref()
                        .map(|e| &e.package_json)
                        .expect("root workspace should have json"),
                )?;
                Ok(lockfile)
            }
        }
    }

    #[tracing::instrument(skip(self))]
    async fn resolve_lockfile(mut self) -> Result<BuildState<'a, ResolvedLockfile, T>, Error> {
        self.connect_internal_dependencies()?;

        let lockfile = match self.populate_lockfile().await {
            Ok(lockfile) => Some(lockfile),
            Err(e) => {
                warn!(
                    "Issues occurred when constructing package graph. Turbo will function, but \
                     some features may not be available: {}",
                    e
                );
                None
            }
        };

        let Self {
            repo_root,
            single,
            workspaces,
            workspace_graph,
            node_lookup,
            package_discovery,
            ..
        } = self;
        Ok(BuildState {
            repo_root,
            single,
            workspaces,
            workspace_graph,
            node_lookup,
            lockfile,
            package_jsons: None,
            state: std::marker::PhantomData,
            package_discovery,
        })
    }
}

impl<'a, T: PackageDiscovery> BuildState<'a, ResolvedLockfile, T> {
    fn all_external_dependencies(&self) -> Result<HashMap<String, HashMap<String, String>>, Error> {
        self.workspaces
            .values()
            .map(|entry| {
                let workspace_path = entry
                    .package_json_path
                    .parent()
                    .unwrap_or(AnchoredSystemPath::new("")?)
                    .to_unix();
                let workspace_string = workspace_path.as_str();
                let external_deps = entry
                    .unresolved_external_dependencies
                    .as_ref()
                    .map(|deps| {
                        deps.iter()
                            .map(|(name, version)| (name.to_string(), version.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok((workspace_string.to_string(), external_deps))
            })
            .collect()
    }

    #[tracing::instrument(skip_all)]
    fn populate_transitive_dependencies(&mut self) -> Result<(), Error> {
        let Some(lockfile) = self.lockfile.as_deref() else {
            return Ok(());
        };

        let mut closures = turborepo_lockfiles::all_transitive_closures(
            lockfile,
            self.all_external_dependencies()?,
        )?;
        for (_, entry) in self.workspaces.iter_mut() {
            entry.transitive_dependencies = closures.remove(&entry.unix_dir_str()?);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn build_inner(mut self) -> Result<PackageGraph, discovery::Error> {
        if let Err(e) = self.populate_transitive_dependencies() {
            warn!("Unable to calculate transitive closures: {}", e);
        }
        let package_manager = self
            .package_discovery
            .discover_packages()
            .instrument(tracing::debug_span!("package discovery"))
            .await?
            .package_manager;
        let Self {
            workspaces,
            workspace_graph,
            node_lookup,
            lockfile,
            ..
        } = self;
        Ok(PackageGraph {
            graph: workspace_graph,
            node_lookup,
            packages: workspaces,
            package_manager,
            lockfile,
        })
    }
}

struct Dependencies {
    internal: HashSet<PackageName>,
    external: BTreeMap<String, String>, // Package name and version
}

impl Dependencies {
    pub fn new<'a, I: IntoIterator<Item = (&'a String, &'a String)>>(
        repo_root: &AbsoluteSystemPath,
        workspace_json_path: &AnchoredSystemPathBuf,
        workspaces: &HashMap<PackageName, PackageInfo>,
        dependencies: I,
    ) -> Self {
        let resolved_workspace_json_path = repo_root.resolve(workspace_json_path);
        let workspace_dir = resolved_workspace_json_path
            .parent()
            .expect("package.json path should have parent");
        let mut internal = HashSet::new();
        let mut external = BTreeMap::new();
        let splitter = DependencySplitter {
            repo_root,
            workspace_dir,
            workspaces,
        };
        for (name, version) in dependencies.into_iter() {
            if let Some(workspace) = splitter.is_internal(name, version) {
                internal.insert(workspace);
            } else {
                external.insert(name.clone(), version.clone());
            }
        }
        Self { internal, external }
    }
}

struct DependencySplitter<'a, 'b, 'c> {
    repo_root: &'a AbsoluteSystemPath,
    workspace_dir: &'b AbsoluteSystemPath,
    workspaces: &'c HashMap<PackageName, PackageInfo>,
}

impl<'a, 'b, 'c> DependencySplitter<'a, 'b, 'c> {
    fn is_internal(&self, name: &str, version: &str) -> Option<PackageName> {
        // TODO implement borrowing for workspaces to allow for zero copy queries
        let workspace_name = PackageName::Other(
            version
                .strip_prefix("workspace:")
                .and_then(|version| version.rsplit_once('@'))
                .filter(|(_, version)| *version == "*" || *version == "^" || *version == "~")
                .map_or(name, |(actual_name, _)| actual_name)
                .to_string(),
        );
        let is_internal = self
            .workspaces
            .get(&workspace_name)
            // This is the current Go behavior, in the future we might not want to paper over a
            // missing version
            .map(|e| e.package_json.version.as_deref().unwrap_or_default())
            .map_or(false, |workspace_version| {
                DependencyVersion::new(version).matches_workspace_package(
                    workspace_version,
                    self.workspace_dir,
                    self.repo_root,
                )
            });
        match is_internal {
            true => Some(workspace_name),
            false => None,
        }
    }
}

struct DependencyVersion<'a> {
    protocol: Option<&'a str>,
    version: &'a str,
}

impl<'a> DependencyVersion<'a> {
    fn new(qualified_version: &'a str) -> Self {
        qualified_version.split_once(':').map_or(
            Self {
                protocol: None,
                version: qualified_version,
            },
            |(protocol, version)| Self {
                protocol: Some(protocol),
                version,
            },
        )
    }

    fn is_external(&self) -> bool {
        // The npm protocol for yarn by default still uses the workspace package if the
        // workspace version is in a compatible semver range. See https://github.com/yarnpkg/berry/discussions/4015
        // For now, we will just assume if the npm protocol is being used and the
        // version matches its an internal dependency which matches the existing
        // behavior before this additional logic was added.

        // TODO: extend this to support the `enableTransparentWorkspaces` yarn option
        self.protocol.map_or(false, |p| p != "npm")
    }

    fn matches_workspace_package(
        &self,
        package_version: &str,
        cwd: &AbsoluteSystemPath,
        root: &AbsoluteSystemPath,
    ) -> bool {
        match self.protocol {
            Some("workspace") => {
                // TODO: Since support at the moment is non-existent for workspaces that contain
                // multiple versions of the same package name, just assume its a
                // match and don't check the range for an exact match.
                true
            }
            Some("file") | Some("link") => {
                // Default to internal if we have the package but somehow cannot get the path
                RelativeUnixPathBuf::new(self.version)
                    .and_then(|file_path| cwd.join_unix_path(file_path))
                    .map_or(true, |dep_path| root.contains(&dep_path))
            }
            Some(_) if self.is_external() => {
                // Other protocols are assumed to be external references ("github:", etc)
                false
            }
            _ if self.version == "*" => true,
            _ => {
                // If we got this far, then we need to check the workspace package version to
                // see it satisfies the dependencies range to determin whether
                // or not its an internal or external dependency.
                let constraint = node_semver::Range::parse(self.version);
                let version = node_semver::Version::parse(package_version);

                // For backwards compatibility with existing behavior, if we can't parse the
                // version then we treat the dependency as an internal package
                // reference and swallow the error.

                // TODO: some package managers also support tags like "latest". Does extra
                // handling need to be added for this corner-case
                constraint
                    .ok()
                    .zip(version.ok())
                    .map_or(true, |(constraint, version)| constraint.satisfies(&version))
            }
        }
    }
}

impl<'a> fmt::Display for DependencyVersion<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.protocol {
            Some(protocol) => f.write_fmt(format_args!("{}:{}", protocol, self.version)),
            None => f.write_str(self.version),
        }
    }
}

impl PackageInfo {
    fn unix_dir_str(&self) -> Result<String, Error> {
        let unix = self
            .package_json_path
            .parent()
            .unwrap_or_else(|| AnchoredSystemPath::new("").expect("empty path is anchored"))
            .to_unix();
        Ok(unix.to_string())
    }
}

#[cfg(test)]
mod test {
    use std::assert_matches::assert_matches;

    use test_case::test_case;
    use turbopath::AbsoluteSystemPathBuf;

    use super::*;

    #[test_case("1.2.3", None, "1.2.3", Some("@scope/foo") ; "handles exact match")]
    #[test_case("1.2.3", None, "^1.0.0", Some("@scope/foo") ; "handles semver range satisfied")]
    #[test_case("2.3.4", None, "^1.0.0", None ; "handles semver range not satisfied")]
    #[test_case("1.2.3", None, "workspace:1.2.3", Some("@scope/foo") ; "handles workspace protocol with version")]
    #[test_case("1.2.3", None, "workspace:*", Some("@scope/foo") ; "handles workspace protocol with no version")]
    #[test_case("1.2.3", None, "workspace:../other-packages/", Some("@scope/foo") ; "handles workspace protocol with relative path")]
    #[test_case("1.2.3", None, "workspace:../@scope/foo", Some("@scope/foo") ; "handles workspace protocol with scoped relative path")]
    #[test_case("1.2.3", None, "npm:^1.2.3", Some("@scope/foo") ; "handles npm protocol with satisfied semver range")]
    #[test_case("2.3.4", None, "npm:^1.2.3", None ; "handles npm protocol with not satisfied semver range")]
    #[test_case("1.2.3", None, "1.2.2-alpha-123abcd.0", None ; "handles pre-release versions")]
    // for backwards compatability with the code before versions were verified
    #[test_case("sometag", None, "1.2.3", Some("@scope/foo") ; "handles non-semver package version")]
    // for backwards compatability with the code before versions were verified
    #[test_case("1.2.3", None, "sometag", Some("@scope/foo") ; "handles non-semver dependency version")]
    #[test_case("1.2.3", None, "file:../libB", Some("@scope/foo") ; "handles file:.. inside repo")]
    #[test_case("1.2.3", None, "file:../../../otherproject", None ; "handles file:.. outside repo")]
    #[test_case("1.2.3", None, "link:../libB", Some("@scope/foo") ; "handles link:.. inside repo")]
    #[test_case("1.2.3", None, "link:../../../otherproject", None ; "handles link:.. outside repo")]
    #[test_case("0.0.0-development", None, "*", Some("@scope/foo") ; "handles development versions")]
    #[test_case("1.2.3", Some("foo"), "workspace:@scope/foo@*", Some("@scope/foo") ; "handles pnpm alias star")]
    #[test_case("1.2.3", Some("foo"), "workspace:@scope/foo@~", Some("@scope/foo") ; "handles pnpm alias tilda")]
    #[test_case("1.2.3", Some("foo"), "workspace:@scope/foo@^", Some("@scope/foo") ; "handles pnpm alias caret")]
    fn test_matches_workspace_package(
        package_version: &str,
        dependency_name: Option<&str>,
        range: &str,
        expected: Option<&str>,
    ) {
        let root = AbsoluteSystemPathBuf::new(if cfg!(windows) {
            "C:\\some\\repo"
        } else {
            "/some/repo"
        })
        .unwrap();
        let pkg_dir = root.join_components(&["packages", "libA"]);
        let workspaces = {
            let mut map = HashMap::new();
            map.insert(
                PackageName::Other("@scope/foo".to_string()),
                PackageInfo {
                    package_json: PackageJson {
                        version: Some(package_version.to_string()),
                        ..Default::default()
                    },
                    package_json_path: AnchoredSystemPathBuf::from_raw("unused").unwrap(),
                    unresolved_external_dependencies: None,
                    transitive_dependencies: None,
                },
            );
            map
        };

        let splitter = DependencySplitter {
            repo_root: &root,
            workspace_dir: &pkg_dir,
            workspaces: &workspaces,
        };

        assert_eq!(
            splitter.is_internal(dependency_name.unwrap_or("@scope/foo"), range),
            expected.map(PackageName::from)
        );
    }

    struct MockDiscovery;
    impl PackageDiscovery for MockDiscovery {
        async fn discover_packages(
            &mut self,
        ) -> Result<crate::discovery::DiscoveryResponse, crate::discovery::Error> {
            Ok(crate::discovery::DiscoveryResponse {
                package_manager: crate::package_manager::PackageManager::Npm,
                workspaces: vec![],
            })
        }
    }

    #[tokio::test]
    async fn test_duplicate_package_names() {
        let root =
            AbsoluteSystemPathBuf::new(if cfg!(windows) { r"C:\repo" } else { "/repo" }).unwrap();
        let builder = PackageGraphBuilder::new(
            &root,
            PackageJson {
                name: Some("root".into()),
                ..Default::default()
            },
        )
        .with_package_discovery(MockDiscovery)
        .with_package_jsons(Some({
            let mut map = HashMap::new();
            map.insert(
                root.join_component("a"),
                PackageJson {
                    name: Some("foo".into()),
                    ..Default::default()
                },
            );
            map.insert(
                root.join_component("b"),
                PackageJson {
                    name: Some("foo".into()),
                    ..Default::default()
                },
            );
            map
        }));
        assert_matches!(builder.build().await, Err(Error::DuplicateWorkspace { .. }))
    }
}
