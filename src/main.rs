use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    fs::File,
    io::{self, BufReader, Write},
    path::Path,
};

use colored::*;
use itertools::Itertools;
use miette::{IntoDiagnostic, Result, WrapErr, bail, miette};
use serde::Deserialize;

use crate::client::{ExecResult, ScoopClient};

mod client;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub scoop_buckets: Vec<ScoopBucket>,
    pub scoop_apps: Vec<ScoopApp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ScoopBucket {
    pub name: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScoopApp {
    pub name: String,
    pub bucket_name: String,
}

// ScoopApp は {bucket_name}/{name} の形式で表示する
impl fmt::Display for ScoopApp {
    fn fmt(&self, b: &mut fmt::Formatter) -> fmt::Result {
        write!(b, "{}/{}", self.bucket_name, self.name)
    }
}

impl<'de> Deserialize<'de> for ScoopApp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        let parts: Vec<&str> = name.splitn(2, '/').collect();
        let Ok([bucket_name, name]): Result<[_; 2], _> = parts.try_into() else {
            return Err(serde::de::Error::custom("invalid format"));
        };

        Ok(ScoopApp {
            name: name.to_string(),
            bucket_name: bucket_name.to_string(),
        })
    }
}

fn make_label(title: &str) -> impl fmt::Display {
    format!("{title:>10}").green().bold()
}

fn make_sublabel(title: &str) -> impl fmt::Display {
    format!("{title:>10}").cyan().bold()
}

fn format_item_add(kind: &str, name: impl fmt::Display) -> String {
    format!("{:>8} {}", kind.green(), name)
}

fn format_item_remove(kind: &str, name: impl fmt::Display) -> String {
    format!("{:>8} {}", kind.red(), name)
}

fn main() -> Result<()> {
    let config = read_config_from_file("app-requirements.yaml")?;
    let mut client = ScoopClient::new().wrap_err("failed to initialize scoop client")?;

    let required =
        get_required_things(&mut client, &config).wrap_err("failed to resolve dependencies")?;
    let installed =
        get_installed_things(&mut client).wrap_err("failed to get installed applications")?;
    let to_uninstall = compute_things_to_uninstall(&installed, &required);
    let to_install = compute_things_to_install(&installed, required);

    if to_uninstall.is_empty() && to_install.is_empty() {
        println!();
        println!("{}", "Everything is up to date!".green().bold());
        return Ok(());
    }

    to_uninstall.describe_plan();
    to_install.describe_plan();

    println!();
    print!("Do you want to proceed? {} ", "[y/N]".cyan());
    io::stdout().flush().into_diagnostic()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).into_diagnostic()?;

    if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes" | "Y") {
        println!("{}", "Operation cancelled.".yellow());
        return Ok(());
    }

    if !to_uninstall.is_empty() {
        println!("{} items", make_label("Uninstalling"));
        uninstall_apps(&mut client, &to_uninstall.scoop_apps)?;
        uninstall_buckets(&mut client, &to_uninstall.scoop_buckets)?;
    }

    if !to_install.is_empty() {
        println!("{} items", make_label("Installing"));
        install_buckets(&mut client, &to_install.scoop_buckets)?;
        install_apps(&mut client, &to_install.scoop_apps)?;
    }

    println!("{}", "Operation completed successfully!".green().bold());

    Ok(())
}

fn read_config_from_file<P: AsRef<Path>>(path: P) -> Result<Config> {
    let path = path.as_ref();
    let file = File::open(path).into_diagnostic().wrap_err_with(|| {
        miette!(
            "failed to read app list from file {path}",
            path = path.display()
        )
    })?;
    let reader = BufReader::new(file);

    serde_yaml::from_reader(reader)
        .into_diagnostic()
        .wrap_err_with(|| {
            miette!(
                "failed to parse app list from file {path}",
                path = path.display()
            )
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequiredThings {
    scoop_buckets: Vec<ScoopBucket>,
    scoop_apps: HashMap<ScoopApp, HashSet<ScoopApp>>,
}

fn get_required_things(client: &mut ScoopClient, config: &Config) -> Result<RequiredThings> {
    println!("{} dependencies", make_label("Loading"));
    fn get_dependencies_of(client: &mut ScoopClient, app: &ScoopApp) -> Result<HashSet<ScoopApp>> {
        println!("{} {}", make_sublabel("Resolving"), app);
        let ExecResult {
            stdout,
            stderr,
            status,
        } = client.exec(&["depends", &app.to_string()])?;

        if !status.success() || stdout.contains("Couldn't find manifest for") {
            bail!("failed to get dependencies for {app}: {stderr}",);
        }

        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .skip(1) // ヘッダー行: Source Name
            .skip(1) // ヘッダー行: ------ ----
            .map(|line| {
                let [bucket_name, name] = line
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .try_into()
                    .map_err(|_| miette!("invalid dependency line: {line}"))?;

                Ok(ScoopApp {
                    bucket_name: bucket_name.to_string(),
                    name: name.to_string(),
                })
            })
            .collect()
    }

    let mut resolved = HashMap::new();
    let mut to_resolve = VecDeque::new();
    to_resolve.extend(config.scoop_apps.to_vec());

    while let Some(app) = to_resolve.pop_front() {
        if resolved.contains_key(&app) {
            continue;
        }

        let dependencies = match get_dependencies_of(client, &app) {
            Ok(deps) => deps,
            Err(e) => {
                println!("{} Skipping due to error: {e}", make_sublabel("Info"));
                continue;
            }
        };

        to_resolve.extend(dependencies.clone());
        resolved.insert(app.clone(), dependencies);
    }

    Ok(RequiredThings {
        scoop_buckets: config.scoop_buckets.clone(),
        scoop_apps: resolved,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstalledThings {
    scoop_buckets: Vec<ScoopBucket>,
    scoop_apps: HashSet<ScoopApp>,
}

// インストールされているアプリケーションのリストを取得
fn get_installed_things(client: &mut ScoopClient) -> Result<InstalledThings> {
    println!("{} currently installed applications", make_label("Loading"));
    let exported = client
        .exec(&["export"])
        .wrap_err("failed to invoke `scoop export`")?;
    if !exported.status.success() {
        bail!("failed to export scoop status: {}", exported.stderr);
    }

    #[derive(Deserialize)]
    struct ExportedScoopData {
        buckets: Vec<ExportedScoopBucket>,
        apps: Vec<ExportedScoopApp>,
    }

    #[derive(Deserialize)]
    struct ExportedScoopBucket {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "Source")]
        source: String,
    }

    #[derive(Deserialize)]
    struct ExportedScoopApp {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "Source")]
        bucket: Option<String>,
    }

    let data: ExportedScoopData = serde_json::from_str(&exported.stdout)
        .into_diagnostic()
        .wrap_err("failed to parse `scoop export` output")?;

    Ok(InstalledThings {
        scoop_buckets: data
            .buckets
            .iter()
            .map(|bucket| ScoopBucket {
                name: bucket.name.clone(),
                source: bucket.source.clone(),
            })
            .collect(),
        scoop_apps: data
            .apps
            .iter()
            .filter_map(|app| {
                app.bucket.as_ref().map(|bucket| ScoopApp {
                    name: app.name.clone(),
                    bucket_name: bucket.clone(),
                })
            })
            .collect(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThingsToUninstall {
    scoop_buckets: HashSet<ScoopBucket>,
    scoop_apps: HashSet<ScoopApp>,
}

impl ThingsToUninstall {
    fn is_empty(&self) -> bool {
        self.scoop_apps.is_empty() && self.scoop_buckets.is_empty()
    }

    fn describe_plan(&self) {
        if self.is_empty() {
            return;
        }

        println!();
        println!("Following items will be {}", "uninstalled".red().bold());

        for bucket in &self.scoop_buckets {
            println!("{}", format_item_remove("bucket", &bucket.name));
        }

        for app in &self.scoop_apps {
            println!("{}", format_item_remove("app", app));
        }
    }
}

fn compute_things_to_uninstall(
    installed_things: &InstalledThings,
    required_things: &RequiredThings,
) -> ThingsToUninstall {
    let mut scoop_buckets = HashSet::new();
    let mut scoop_apps = HashSet::new();

    for bucket in &installed_things.scoop_buckets {
        if !required_things.scoop_buckets.contains(bucket) {
            scoop_buckets.insert(bucket.clone());
        }
    }

    for app in &installed_things.scoop_apps {
        if !required_things.scoop_apps.contains_key(app) {
            scoop_apps.insert(app.clone());
        }
    }

    ThingsToUninstall {
        scoop_buckets,
        scoop_apps,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThingsToInstall {
    scoop_buckets: HashSet<ScoopBucket>,
    scoop_apps: HashSet<ScoopApp>,
}

impl ThingsToInstall {
    fn is_empty(&self) -> bool {
        self.scoop_apps.is_empty() && self.scoop_buckets.is_empty()
    }

    fn describe_plan(&self) {
        if self.is_empty() {
            return;
        }

        println!();
        println!("Following items will be {}", "installed".green().bold());

        for bucket in &self.scoop_buckets {
            println!("{}", format_item_add("bucket", &bucket.name));
        }

        for app in &self.scoop_apps {
            println!("{}", format_item_add("app", app));
        }
    }
}

fn compute_things_to_install(
    installed_things: &InstalledThings,
    required_things: RequiredThings,
) -> ThingsToInstall {
    let mut scoop_buckets = HashSet::new();
    let mut scoop_apps = HashSet::new();

    for bucket in required_things.scoop_buckets {
        if !installed_things.scoop_buckets.contains(&bucket) {
            scoop_buckets.insert(bucket);
        }
    }

    for app in required_things.scoop_apps.keys() {
        if !installed_things.scoop_apps.contains(app) {
            scoop_apps.insert(app.clone());
        }
    }

    ThingsToInstall {
        scoop_buckets,
        scoop_apps,
    }
}

fn uninstall_buckets<'a>(
    client: &mut ScoopClient,
    buckets: impl IntoIterator<Item = &'a ScoopBucket>,
) -> Result<()> {
    let mut args = vec!["bucket", "rm"];
    let bucket_names = buckets.into_iter().map(|b| &*b.name).collect_vec();
    if bucket_names.is_empty() {
        return Ok(()); // Nothing to uninstall
    }
    args.extend(&bucket_names);
    let output = client.exec(&args).wrap_err("failed to uninstall buckets")?;
    if !output.status.success() {
        bail!("failed to uninstall buckets: {}", output.stderr.trim());
    }

    Ok(())
}

fn uninstall_apps<'a>(
    client: &mut ScoopClient,
    apps: impl IntoIterator<Item = &'a ScoopApp>,
) -> Result<()> {
    let mut args = vec!["uninstall"];
    let app_ids = apps.into_iter().map(|app| app.to_string()).collect_vec();
    args.extend(app_ids.iter().map(|id| id.as_str()));
    if app_ids.is_empty() {
        return Ok(()); // Nothing to uninstall
    }
    let output = client
        .exec(&args)
        .wrap_err("failed to uninstall applications")?;
    if !output.status.success() {
        bail!("failed to uninstall applications: {}", output.stderr.trim());
    }

    Ok(())
}

fn install_buckets<'a>(
    client: &mut ScoopClient,
    buckets: impl IntoIterator<Item = &'a ScoopBucket>,
) -> Result<()> {
    for bucket in buckets {
        let output = client
            .exec(&["bucket", "add", &bucket.name, &bucket.source])
            .wrap_err_with(|| {
                miette!(
                    "failed to install bucket {} from {}",
                    bucket.name,
                    bucket.source
                )
            })?;
        if !output.status.success() {
            bail!(
                "failed to install bucket {}: {}",
                bucket.name,
                output.stderr.trim()
            );
        }
    }

    Ok(())
}

fn install_apps<'a>(
    client: &mut ScoopClient,
    apps: impl IntoIterator<Item = &'a ScoopApp>,
) -> Result<()> {
    let mut args = vec!["install"];
    let app_ids = apps.into_iter().map(|app| app.to_string()).collect_vec();
    args.extend(app_ids.iter().map(|id| id.as_str()));
    let output = client
        .exec(&args)
        .wrap_err("failed to install applications")?;
    if !output.status.success() {
        bail!("failed to install applications: {}", output.stderr.trim());
    }

    Ok(())
}
