use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    fs::File,
    io::{self, BufReader, Write},
    path::Path,
    process::Command,
};

use colored::*;
use miette::{IntoDiagnostic, Result, WrapErr, bail, miette};
use serde::Deserialize;

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
    let required = get_required_things(&config).wrap_err("failed to resolve dependencies")?;
    let installed = get_installed_things().wrap_err("failed to get installed applications")?;
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
        uninstall_apps(&to_uninstall.scoop_apps)?;
        uninstall_buckets(&to_uninstall.scoop_buckets)?;
    }

    if !to_install.is_empty() {
        println!("{} items", make_label("Installing"));
        install_buckets(&to_install.scoop_buckets)?;
        install_apps(&to_install.scoop_apps)?;
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

fn get_required_things(config: &Config) -> Result<RequiredThings> {
    println!("{} dependencies", make_label("Loading"));
    fn resolve_dependencies_for(app: &ScoopApp) -> Result<HashSet<ScoopApp>> {
        println!("{} {}", make_sublabel("Resolving"), app);
        let output = Command::new("scoop")
            .arg("depends")
            .arg(&app.name)
            .output()
            .into_diagnostic()
            .wrap_err_with(|| miette!("failed to invoke `scoop depends {app}`"))?;

        if !output.status.success() {
            bail!(
                "failed to get dependencies for {app}: {stderr}",
                stderr = String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

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

        let dependencies = resolve_dependencies_for(&app).wrap_err_with(|| {
            miette!("failed to resolve dependencies for {name}", name = app.name)
        })?;

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
fn get_installed_things() -> Result<InstalledThings> {
    println!("{} currently installed applications", make_label("Loading"));
    let exported = Command::new("scoop")
        .arg("export")
        .output()
        .into_diagnostic()
        .wrap_err("failed to invoke `scoop export`")?;
    if !exported.status.success() {
        bail!(
            "failed to export scoop status: {}",
            String::from_utf8_lossy(&exported.stderr)
        );
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

    let data: ExportedScoopData = serde_json::from_slice(&exported.stdout)
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

fn uninstall_buckets<'a>(buckets: impl IntoIterator<Item = &'a ScoopBucket>) -> Result<()> {
    Command::new("scoop")
        .arg("bucket")
        .arg("rm")
        .args(buckets.into_iter().map(|bucket| bucket.name.clone()))
        .output()
        .into_diagnostic()
        .wrap_err("failed to uninstall buckets")?;

    Ok(())
}

fn uninstall_apps<'a>(apps: impl IntoIterator<Item = &'a ScoopApp>) -> Result<()> {
    Command::new("scoop")
        .arg("uninstall")
        .args(apps.into_iter().map(|app| app.to_string()))
        .output()
        .into_diagnostic()
        .wrap_err("failed to uninstall applications")?;

    Ok(())
}

fn install_buckets<'a>(buckets: impl IntoIterator<Item = &'a ScoopBucket>) -> Result<()> {
    Command::new("scoop")
        .arg("bucket")
        .arg("add")
        .args(buckets.into_iter().map(|bucket| bucket.name.clone()))
        .output()
        .into_diagnostic()
        .wrap_err("failed to install buckets")?;

    Ok(())
}

fn install_apps<'a>(apps: impl IntoIterator<Item = &'a ScoopApp>) -> Result<()> {
    Command::new("scoop")
        .arg("install")
        .args(apps.into_iter().map(|app| app.to_string()))
        .output()
        .into_diagnostic()
        .wrap_err("failed to install applications")?;

    Ok(())
}
