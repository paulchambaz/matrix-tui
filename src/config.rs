//! The list of servers and how to authenticate to each.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// How a server is signed in to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    #[default]
    Password,
    Sso,
    Oidc,
}

/// One configured server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "is_password")]
    pub method: Method,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_cmd: Option<String>,
}

/// What the client presents to the server.
#[derive(Debug, Clone)]
pub enum Credentials {
    /// Login and password, exchanged for a session that can be renewed.
    Password { login: String, password: String },
    /// A login in the web browser, started by the given program.
    Sso { browser: String },
    /// A login in the web browser through OAuth 2.0, started by the given program.
    Oidc { browser: String },
}

/// The programs that open links and files.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Open {
    pub image: String,
    pub video: String,
    pub browser: String,
    pub text: String,
    pub other: String,
}

impl Default for Open {
    fn default() -> Self {
        let any = || "xdg-open".to_owned();
        Self {
            image: any(),
            video: any(),
            browser: any(),
            text: any(),
            other: any(),
        }
    }
}

/// The user's choices, read from `config.toml`; every key is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub editor: Option<String>,
    pub copy: Option<String>,
    pub notify: Option<String>,
    pub open: Open,
}

impl Settings {
    /// The program `ctrl+g` runs: the setting, else `$EDITOR`, else `vi`.
    pub fn editor(&self, environment: Option<&str>) -> String {
        let chosen = self.editor.as_deref().filter(|e| !e.is_empty());
        let fallback = environment.filter(|e| !e.is_empty());
        chosen.or(fallback).unwrap_or("vi").to_owned()
    }
}

/// Where the settings live.
///
/// # Errors
/// Fails when no home directory can be found.
pub fn settings_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "mx").context("no home directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Reads the settings; a missing file means the defaults.
///
/// # Errors
/// Fails when the file cannot be read or parsed.
pub fn load_settings(path: &Path) -> Result<Settings> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "server")]
    servers: Vec<ServerConfig>,
}

impl ServerConfig {
    /// Resolves the secret, running its command when one is configured.
    ///
    /// # Errors
    /// Fails when the secret is missing, given twice, or its command fails.
    pub fn credentials(&self, browser: &str) -> Result<Credentials> {
        if self.method != Method::Password {
            let word = if self.method == Method::Sso {
                "sso"
            } else {
                "oidc"
            };
            if !self.login.is_empty() || self.password.is_some() || self.password_cmd.is_some() {
                bail!("server {}: {word} takes no login or password", self.name);
            }
            let browser = browser.to_owned();
            return Ok(match self.method {
                Method::Sso => Credentials::Sso { browser },
                _ => Credentials::Oidc { browser },
            });
        }
        match (&self.password, &self.password_cmd) {
            (Some(password), None) => self.password_of(password.clone()),
            (None, Some(cmd)) => self.password_of(run(cmd)?),
            _ => bail!(
                "server {}: set exactly one of password, password_cmd",
                self.name
            ),
        }
    }

    fn password_of(&self, password: String) -> Result<Credentials> {
        if self.login.is_empty() {
            bail!("server {}: a password needs a login", self.name);
        }
        Ok(Credentials::Password {
            login: self.login.clone(),
            password,
        })
    }
}

fn is_password(method: &Method) -> bool {
    *method == Method::Password
}

fn run(cmd: &str) -> Result<String> {
    let out = Command::new("sh")
        .args(["-c", cmd])
        .output()
        .context("running credential command")?;
    if !out.status.success() {
        bail!("credential command failed");
    }
    Ok(String::from_utf8(out.stdout)
        .context("credential command output is not UTF-8")?
        .trim_end()
        .to_owned())
}

/// Where the server list lives.
///
/// # Errors
/// Fails when no home directory can be found.
pub fn default_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "mx").context("no home directory")?;
    Ok(dirs.config_dir().join("servers.toml"))
}

/// Reads the server list; a missing file is an empty list.
///
/// # Errors
/// Fails when the file cannot be read or parsed.
pub fn load(path: &Path) -> Result<Vec<ServerConfig>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(toml::from_str::<File>(&text)
            .with_context(|| format!("parsing {}", path.display()))?
            .servers),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes the server list readable by its owner only.
///
/// # Errors
/// Fails when the file cannot be written.
pub fn save(path: &Path, servers: &[ServerConfig]) -> Result<()> {
    let text = toml::to_string(&File {
        servers: servers.to_vec(),
    })?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(text.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn server() -> ServerConfig {
        ServerConfig {
            name: "work".into(),
            url: "https://chat.example.org".into(),
            method: Method::Password,
            login: "paul".into(),
            password: Some("hunter2".into()),
            password_cmd: None,
        }
    }

    #[test]
    fn round_trip_keeps_owner_only_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mx/servers.toml");
        save(&path, &[server()]).unwrap();
        assert_eq!(load(&path).unwrap(), vec![server()]);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("none.toml")).unwrap().is_empty());
    }

    #[test]
    fn two_secrets_are_rejected() {
        let mut s = server();
        s.password_cmd = Some("echo x".into());
        assert!(s.credentials("xdg-open").is_err());
    }

    #[test]
    fn password_cmd_is_run() {
        let mut s = server();
        s.password = None;
        s.password_cmd = Some("echo secret".into());
        match s.credentials("xdg-open").unwrap() {
            Credentials::Password { password, .. } => assert_eq!(password, "secret"),
            _ => panic!("expected a password"),
        }
    }

    #[test]
    fn sso_gives_the_browser_and_takes_no_secret() {
        let mut s = server();
        s.method = Method::Sso;
        assert!(s.credentials("firefox").is_err());
        s.login = String::new();
        s.password = None;
        match s.credentials("firefox").unwrap() {
            Credentials::Sso { browser } => assert_eq!(browser, "firefox"),
            _ => panic!("expected sso"),
        }
    }

    #[test]
    fn oidc_gives_the_browser_and_takes_no_secret() {
        let mut s = server();
        s.method = Method::Oidc;
        assert!(s.credentials("firefox").is_err());
        s.login = String::new();
        s.password = None;
        match s.credentials("firefox").unwrap() {
            Credentials::Oidc { browser } => assert_eq!(browser, "firefox"),
            _ => panic!("expected oidc"),
        }
    }

    #[test]
    fn the_method_is_read_from_the_file_and_left_out_when_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mx/servers.toml");
        let mut s = server();
        s.method = Method::Sso;
        s.login = String::new();
        s.password = None;
        save(&path, &[s.clone(), server()]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("method").count(), 1);
        assert!(text.contains("method = \"sso\""));
        assert_eq!(load(&path).unwrap(), vec![s, server()]);
    }

    #[test]
    fn settings_default_to_xdg_open_and_the_environment_editor() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load_settings(&dir.path().join("none.toml")).unwrap();
        assert_eq!(settings.open, Open::default());
        assert_eq!(settings.open.image, "xdg-open");
        assert_eq!(settings.editor(Some("nano")), "nano");
        assert_eq!(settings.editor(None), "vi");
        assert_eq!(settings.editor(Some("")), "vi");
    }

    #[test]
    fn a_partial_file_keeps_the_rest_and_the_editor_setting_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "editor = \"vi\"\n[open]\nimage = \"nsxiv\"\ntext = \"nvim -R\"\n",
        )
        .unwrap();
        let settings = load_settings(&path).unwrap();
        assert_eq!(settings.open.image, "nsxiv");
        assert_eq!(settings.open.text, "nvim -R");
        assert_eq!(settings.open.video, "xdg-open");
        assert_eq!(settings.editor(Some("nano")), "vi");
        assert_eq!(settings.copy, None);
    }

    #[test]
    fn a_typo_or_a_broken_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "edtor = \"vi\"\n").unwrap();
        assert!(load_settings(&path).is_err());
        fs::write(&path, "editor = \n").unwrap();
        assert!(load_settings(&path).is_err());
    }
}
