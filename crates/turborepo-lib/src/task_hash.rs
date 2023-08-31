use std::collections::{HashMap, HashSet};

use serde::Serialize;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::debug;
use turbopath::{AbsoluteSystemPath, AnchoredSystemPath, AnchoredSystemPathBuf};
use turborepo_env::{BySource, DetailedMap, EnvironmentVariableMap, ResolvedEnvMode};
use turborepo_scm::SCM;

use crate::{
    engine::TaskNode,
    framework::infer_framework,
    hash::{FileHashes, TaskHashable, TurboHash},
    opts::Opts,
    package_graph::{WorkspaceInfo, WorkspaceName},
    run::task_id::{TaskId, ROOT_PKG_NAME},
    task_graph::TaskDefinition,
};

#[derive(Debug, Error)]
pub enum Error {
    #[error("missing pipeline entry {0}")]
    MissingPipelineEntry(TaskId<'static>),
    #[error("missing package.json for {0}")]
    MissingPackageJson(String),
    #[error("cannot find package-file hash for {0}")]
    MissingPackageFileHash(String),
    #[error("missing hash for dependent task {0}")]
    MissingDependencyTaskHash(String),
    #[error(transparent)]
    SCM(#[from] turborepo_scm::Error),
    #[error(transparent)]
    Env(#[from] turborepo_env::Error),
    #[error(transparent)]
    Regex(#[from] regex::Error),
    #[error(transparent)]
    Path(#[from] turbopath::PathError),
}

#[derive(Debug)]
struct PackageFileHashInputs<'a> {
    task_id: TaskId<'static>,
    task_definition: &'a TaskDefinition,
    workspace_name: WorkspaceName,
}

impl TaskHashable<'_> {
    fn calculate_task_hash(mut self) -> String {
        if matches!(self.env_mode, ResolvedEnvMode::Loose) {
            self.pass_through_env = &[];
        }

        self.hash()
    }
}

#[derive(Debug, Default, Serialize)]
pub struct PackageInputsHashes {
    // We make the TaskId a String for serialization purposes
    hashes: HashMap<String, String>,
    expanded_hashes: HashMap<String, FileHashes>,
}

impl PackageInputsHashes {
    pub fn calculate_file_hashes<'a>(
        scm: SCM,
        all_tasks: impl Iterator<Item = &'a TaskNode>,
        workspaces: HashMap<&WorkspaceName, &WorkspaceInfo>,
        task_definitions: &HashMap<TaskId<'static>, TaskDefinition>,
        repo_root: &AbsoluteSystemPath,
    ) -> Result<PackageInputsHashes, Error> {
        let mut hash_tasks = Vec::new();

        for task in all_tasks {
            let TaskNode::Task(task_id) = task else {
                continue;
            };

            if task_id.package() == ROOT_PKG_NAME {
                continue;
            }

            let task_definition = task_definitions
                .get(&task_id)
                .ok_or_else(|| Error::MissingPipelineEntry(task_id.clone()))?;

            // TODO: Look into making WorkspaceName take a Cow
            let workspace_name = WorkspaceName::Other(task_id.package().to_string());

            let package_file_hash_inputs = PackageFileHashInputs {
                task_id: task_id.clone(),
                task_definition,
                workspace_name,
            };

            hash_tasks.push(package_file_hash_inputs);
        }

        let mut hashes = HashMap::with_capacity(hash_tasks.len());
        let mut hash_objects = HashMap::with_capacity(hash_tasks.len());

        for package_file_hash_inputs in hash_tasks {
            let pkg = workspaces
                .get(&package_file_hash_inputs.workspace_name)
                .ok_or_else(|| {
                    Error::MissingPackageJson(package_file_hash_inputs.workspace_name.to_string())
                })?;

            let package_path = pkg
                .package_json_path
                .parent()
                .unwrap_or_else(|| AnchoredSystemPath::new("").unwrap());

            let mut hash_object = scm.get_package_file_hashes(
                &repo_root,
                package_path,
                &package_file_hash_inputs.task_definition.inputs,
            )?;

            if !package_file_hash_inputs.task_definition.dot_env.is_empty() {
                let package_path = pkg
                    .package_json_path
                    .parent()
                    .unwrap_or_else(|| AnchoredSystemPath::new("").unwrap());
                let absolute_package_path = repo_root.resolve(package_path);
                let dot_env_object = scm.hash_existing_of(
                    &absolute_package_path,
                    package_file_hash_inputs
                        .task_definition
                        .dot_env
                        .iter()
                        .map(|p| p.to_anchored_system_path_buf()),
                )?;

                for (key, value) in dot_env_object {
                    hash_object.insert(key, value);
                }
            }

            let file_hashes = FileHashes(hash_object);
            let hash = file_hashes.clone().hash();

            hashes.insert(package_file_hash_inputs.task_id.to_string(), hash);
            hash_objects.insert(package_file_hash_inputs.task_id.to_string(), file_hashes);
        }

        Ok(PackageInputsHashes {
            hashes: hashes,
            expanded_hashes: hash_objects,
        })
    }
}

#[derive(Default, Debug, Serialize)]
pub struct TaskHashTracker {
    #[serde(skip)]
    package_task_env_vars: HashMap<TaskId<'static>, DetailedMap>,
    package_task_hashes: HashMap<TaskId<'static>, String>,
    #[serde(skip)]
    package_task_framework: HashMap<TaskId<'static>, String>,
    #[serde(skip)]
    package_task_outputs: HashMap<TaskId<'static>, Vec<AnchoredSystemPathBuf>>,
}

/// Caches package-inputs hashes, and package-task hashes.
pub struct TaskHasher<'a> {
    package_inputs_hashes: PackageInputsHashes,
    opts: &'a Opts<'a>,
    env_at_execution_start: &'a EnvironmentVariableMap,
    global_hash: &'a str,

    task_hash_tracker: Mutex<TaskHashTracker>,
}

impl<'a> TaskHasher<'a> {
    pub fn new(
        package_inputs_hashes: PackageInputsHashes,
        opts: &'a Opts,
        env_at_execution_start: &'a EnvironmentVariableMap,
        global_hash: &'a str,
    ) -> Self {
        Self {
            package_inputs_hashes,
            opts,
            env_at_execution_start,
            global_hash,
            task_hash_tracker: Mutex::new(TaskHashTracker::default()),
        }
    }

    pub async fn calculate_task_hash(
        &self,
        task_id: &TaskId<'static>,
        task_definition: &TaskDefinition,
        task_env_mode: ResolvedEnvMode,
        workspace: &WorkspaceInfo,
        dependency_set: HashSet<&TaskNode>,
    ) -> Result<String, Error> {
        let do_framework_inference = self.opts.run_opts.framework_inference;
        let is_monorepo = !self.opts.run_opts.single_package;

        let hash_of_files = self
            .package_inputs_hashes
            .hashes
            .get(&task_id.to_string())
            .ok_or_else(|| Error::MissingPackageFileHash(task_id.to_string()))?;
        let mut explicit_env_var_map = EnvironmentVariableMap::default();
        let mut all_env_var_map = EnvironmentVariableMap::default();
        let mut matching_env_var_map = EnvironmentVariableMap::default();

        if do_framework_inference {
            // Se if we infer a framework
            if let Some(framework) = infer_framework(workspace, is_monorepo) {
                debug!("auto detected framework for {}", task_id.package());
                debug!(
                    "framework: {}, env_prefix: {:?}",
                    framework.slug(),
                    framework.env_wildcards()
                );
                let mut computed_wildcards = framework
                    .env_wildcards()
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>();

                if let Some(exclude_prefix) =
                    self.env_at_execution_start.get("TURBOREPO_EXCLUDE_PREFIX")
                {
                    if !exclude_prefix.is_empty() {
                        let computed_exclude = format!("!{}*", exclude_prefix);
                        debug!(
                            "excluding environment variables matching wildcard {}",
                            computed_exclude
                        );
                        computed_wildcards.push(computed_exclude);
                    }
                }

                let inference_env_var_map = self
                    .env_at_execution_start
                    .from_wildcards(&computed_wildcards)?;

                let user_env_var_set = self
                    .env_at_execution_start
                    .wildcard_map_from_wildcards_unresolved(&task_definition.env)?;

                all_env_var_map.union(&user_env_var_set.inclusions);
                all_env_var_map.union(&inference_env_var_map);
                all_env_var_map.difference(&user_env_var_set.exclusions);

                explicit_env_var_map.union(&user_env_var_set.inclusions);
                explicit_env_var_map.difference(&user_env_var_set.exclusions);

                matching_env_var_map.union(&inference_env_var_map);
                matching_env_var_map.difference(&user_env_var_set.exclusions);
            } else {
                let all_env_var_map = self
                    .env_at_execution_start
                    .from_wildcards(&task_definition.env)?;

                explicit_env_var_map.union(&all_env_var_map);
            }
        } else {
            all_env_var_map = self
                .env_at_execution_start
                .from_wildcards(&task_definition.env)?;

            explicit_env_var_map.union(&mut all_env_var_map);
        }

        let env_vars = DetailedMap {
            all: all_env_var_map,
            by_source: BySource {
                explicit: explicit_env_var_map,
                matching: matching_env_var_map,
            },
        };

        let hashable_env_pairs = env_vars.all.to_hashable();
        let outputs = task_definition.hashable_outputs(&task_id);
        let task_dependency_hashes = self.calculate_dependency_hashes(dependency_set).await?;

        debug!(
            "task hash env vars for {}:{}\n vars: {:?}",
            task_id.package(),
            task_id.task(),
            hashable_env_pairs
        );

        let task_hashable = TaskHashable {
            global_hash: self.global_hash,
            task_dependency_hashes,
            package_dir: workspace.package_path().to_unix(),
            hash_of_files,
            external_deps_hash: workspace.get_external_deps_hash(),
            task: task_id.task(),
            outputs,

            pass_through_args: self.opts.run_opts.pass_through_args,
            env: &task_definition.env,
            resolved_env_vars: hashable_env_pairs,
            pass_through_env: &task_definition.pass_through_env,
            env_mode: task_env_mode,
            dot_env: &task_definition.dot_env,
        };

        println!("task hashable: {:#?}", task_hashable);

        let task_hash = task_hashable.hash();

        let mut task_hash_tracker = self.task_hash_tracker.lock().await;
        task_hash_tracker
            .package_task_env_vars
            .insert(task_id.clone(), env_vars);
        task_hash_tracker
            .package_task_hashes
            .insert(task_id.clone(), task_hash.clone());

        Ok(task_hash)
    }

    async fn calculate_dependency_hashes(
        &self,
        dependency_set: HashSet<&TaskNode>,
    ) -> Result<Vec<String>, Error> {
        let mut dependency_hash_set = HashSet::new();

        for dependency_task in dependency_set {
            let TaskNode::Task(dependency_task_id) = dependency_task else {
                continue;
            };

            if dependency_task_id.package() == ROOT_PKG_NAME {
                continue;
            }

            let task_hash_tracker = self.task_hash_tracker.lock().await;
            let dependency_hash = task_hash_tracker
                .package_task_hashes
                .get(&dependency_task_id)
                .ok_or_else(|| Error::MissingDependencyTaskHash(dependency_task.to_string()))?;
            dependency_hash_set.insert(dependency_hash.clone());
        }

        let mut dependency_hash_list = dependency_hash_set.into_iter().collect::<Vec<_>>();
        dependency_hash_list.sort();

        Ok(dependency_hash_list)
    }

    pub fn into_tracker(self) -> TaskHashTracker {
        self.task_hash_tracker.into_inner()
    }
}
