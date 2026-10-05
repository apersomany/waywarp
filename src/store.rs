// Where instances keep their state, and how commands find them: by index, the primary key, or by
// a name assigned with `up --name`.
use anyhow::{Context, Result, bail};
use nix::unistd::Uid;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

// Root and each unprivileged account have separate stores, and so separate index spaces.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Store {
    state: PathBuf,
    runtime: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instance {
    pub index: u8,
    pub store: Store,
}

// Unique within a store and valid in systemd unit names; the leading letter keeps names apart
// from indices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl FromStr for Name {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut bytes = value.bytes();
        let valid = value.len() <= 32
            && bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !valid {
            return Err(format!(
                "instance name {value:?} must be up to 32 lowercase letters, digits, and hyphens, starting with a letter"
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for Name {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

// An instance as a command names it: digits select an index, anything else a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    Index(u8),
    Name(Name),
}

impl FromStr for Selector {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            return value
                .parse()
                .map(Self::Index)
                .map_err(|_| format!("instance index {value} is not between 0 and 255"));
        }
        value.parse().map(Self::Name)
    }
}

fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = path.with_extension(format!("{}.{nonce}.new", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(path.parent().context("file has no parent directory")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

impl Store {
    pub fn current() -> Result<Self> {
        if Uid::effective().is_root() {
            return Ok(Self {
                state: "/var/lib/waywarp".into(),
                runtime: "/run/waywarp".into(),
            });
        }
        let variable = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let home = variable("HOME").context("HOME is not set")?;
        let state = variable("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        let runtime = variable("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
        if !state.is_absolute() || !runtime.is_absolute() {
            bail!("XDG_STATE_HOME and XDG_RUNTIME_DIR must be absolute");
        }
        Ok(Self {
            state: state.join("waywarp"),
            runtime: runtime.join("waywarp"),
        })
    }

    pub fn mudfish_auth_limit(&self) -> Result<PathBuf> {
        private_directory(&self.runtime)?;
        Ok(self.runtime.join("mudfish-auth"))
    }

    pub fn instance(&self, index: u8) -> Instance {
        Instance {
            index,
            store: self.clone(),
        }
    }

    // Shared by every instance in the store, such as Cloudflare's location data.
    pub fn cache(&self, file: &str) -> PathBuf {
        self.state.join(file)
    }

    fn indices(directory: &Path) -> Result<Vec<u8>> {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut indices = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            if let Ok(Selector::Index(index)) = name.to_string_lossy().parse() {
                indices.push(index);
            }
        }
        indices.sort_unstable();
        Ok(indices)
    }

    pub fn resolve(&self, selector: &Selector) -> Result<Instance> {
        match selector {
            Selector::Index(index) => Ok(self.instance(*index)),
            Selector::Name(name) => self.named(name)?.with_context(|| {
                format!("no instance is named {name}; assign it with `waywarp up proxy INDEX --name {name}`")
            }),
        }
    }

    fn named(&self, name: &Name) -> Result<Option<Instance>> {
        for index in Self::indices(&self.state)? {
            let instance = self.instance(index);
            if instance.name()?.as_ref() == Some(name) {
                return Ok(Some(instance));
            }
        }
        Ok(None)
    }

    // Gives `instance` its name, replacing any earlier one; a store-wide lock keeps two
    // commands from claiming one name at once.
    pub fn assign(&self, instance: &Instance, name: &Name) -> Result<()> {
        private_directory(&self.state)?;
        let names = File::create(self.state.join("names.lock"))?;
        names.lock()?;
        if let Some(owner) = self.named(name)?
            && owner.index != instance.index
        {
            bail!("the name {name} belongs to instance {}", owner.index);
        }
        private_directory(&instance.state())?;
        atomic_write(&instance.state().join("name"), name.to_string().as_bytes())
    }

    pub fn running(&self) -> Result<Vec<Instance>> {
        Ok(Self::indices(&self.runtime)?
            .into_iter()
            .map(|index| self.instance(index))
            .filter(|instance| instance.control().exists())
            .collect())
    }
}

// Taken before a command changes an instance and held until its supervisor exits. The lock
// belongs to the open file, so it follows the descriptor into a detached supervisor, and the
// kernel releases it however the last holder ends.
pub struct Lock(File);

impl From<Lock> for OwnedFd {
    fn from(lock: Lock) -> Self {
        lock.0.into()
    }
}

impl From<OwnedFd> for Lock {
    fn from(descriptor: OwnedFd) -> Self {
        Self(descriptor.into())
    }
}

impl Instance {
    pub fn state(&self) -> PathBuf {
        self.store.state.join(self.index.to_string())
    }

    pub fn runtime(&self) -> PathBuf {
        self.store.runtime.join(self.index.to_string())
    }

    pub fn control(&self) -> PathBuf {
        self.runtime().join("control")
    }

    pub fn log(&self) -> PathBuf {
        self.state().join("waywarp.log")
    }

    pub fn registration(&self) -> PathBuf {
        self.state().join("registration")
    }

    pub fn warp_logs(&self) -> PathBuf {
        self.state().join("logs")
    }

    pub fn daemon_socket(&self) -> PathBuf {
        self.runtime().join("daemon")
    }

    pub fn resolv_conf(&self) -> PathBuf {
        self.runtime().join("resolv.conf")
    }

    pub fn name(&self) -> Result<Option<Name>> {
        let path = self.state().join("name");
        match fs::read_to_string(&path) {
            Ok(name) => Ok(Some(
                name.trim()
                    .parse()
                    .map_err(anyhow::Error::msg)
                    .with_context(|| format!("reading {}", path.display()))?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn lock(&self) -> Result<Lock> {
        private_directory(&self.store.runtime)?;
        private_directory(&self.runtime())?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.runtime().join("lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Lock(file)),
            Err(TryLockError::WouldBlock) => bail!(
                "instance {} is running or starting; stop it with `waywarp down {}`",
                self.index,
                self.index
            ),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    // Callers hold the instance lock, including callers checking whether consent is needed.
    pub fn recover_registration(&self) -> Result<()> {
        let target = self.registration();
        let temporary = self.state().join(".registration-import");
        let backup = self.state().join(".registration-backup");
        if !target.join("reg.json").is_file() && backup.join("reg.json").is_file() {
            if target.exists() {
                fs::remove_dir_all(&target)?;
            }
            fs::rename(&backup, &target)?;
            File::open(self.state())?.sync_all()?;
        } else if target.join("reg.json").is_file() && backup.exists() {
            File::open(self.state())?.sync_all()?;
            fs::remove_dir_all(&backup)?;
            File::open(self.state())?.sync_all()?;
        }
        if temporary.exists() {
            fs::remove_dir_all(temporary)?;
        }
        Ok(())
    }

    pub fn registration_edge(&self, port: u16) -> Result<Option<Ipv4Addr>> {
        self.recover_registration()?;
        #[derive(Deserialize)]
        struct Configuration {
            account: Account,
            endpoints: Vec<Endpoint>,
        }
        #[derive(Deserialize)]
        struct Account {
            account_type: String,
        }
        #[derive(Deserialize)]
        struct Endpoint {
            v4: String,
        }

        let path = self.registration().join("conf.json");
        let contents = match fs::read(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let configuration: Configuration = serde_json::from_slice(&contents)
            .with_context(|| format!("reading {}", path.display()))?;
        if !configuration
            .account
            .account_type
            .eq_ignore_ascii_case("team")
        {
            return Ok(None);
        }
        configuration
            .endpoints
            .into_iter()
            .filter_map(|endpoint| endpoint.v4.parse::<SocketAddrV4>().ok())
            .find(|endpoint| endpoint.port() == port)
            .map(|endpoint| Some(*endpoint.ip()))
            .with_context(|| format!("Team registration has no IPv4 edge on port {port}"))
    }

    // Callers hold the instance lock, so no supervisor is using the registration.
    pub fn import_registration(&self, source: &Path, replace: bool) -> Result<()> {
        const FILES: &[&str] = &[
            "reg.json",
            "conf.json",
            "settings.json",
            "consumer-settings.json",
            "final-overrides-settings.json",
            "reg_mdm_orgs.json",
            "warp.db",
        ];

        self.recover_registration()?;
        let target = self.registration();
        if target.join("reg.json").is_file() && !replace {
            bail!(
                "instance {} already has a registration; pass --replace to overwrite it",
                self.index
            );
        }
        if !source.join("reg.json").is_file() {
            bail!("{} contains no WARP registration", source.display());
        }
        private_directory(&self.store.state)?;
        private_directory(&self.state())?;
        let temporary = self.state().join(".registration-import");
        let backup = self.state().join(".registration-backup");
        private_directory(&temporary)?;
        for name in FILES {
            let from = source.join(name);
            if !from.exists() {
                continue;
            }
            if !from.is_file() {
                bail!("{} is not a regular file", from.display());
            }
            let to = temporary.join(name);
            fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
            fs::set_permissions(&to, fs::Permissions::from_mode(0o600))?;
            File::open(&to)?.sync_all()?;
        }
        File::open(&temporary)?.sync_all()?;
        if target.exists() {
            fs::rename(&target, &backup)?;
            File::open(self.state())?.sync_all()?;
        }
        if let Err(error) = fs::rename(&temporary, &target) {
            if backup.exists() {
                let _ = fs::rename(&backup, &target);
            }
            return Err(error.into());
        }
        File::open(self.state())?.sync_all()?;
        if backup.exists() {
            fs::remove_dir_all(backup)?;
            File::open(self.state())?.sync_all()?;
        }
        Ok(())
    }

    // Callers hold the instance lock, so nothing else is using these paths.
    pub fn prepare(&self) -> Result<()> {
        self.recover_registration()?;
        for path in [
            &self.store.state,
            &self.state(),
            &self.registration(),
            &self.warp_logs(),
        ] {
            private_directory(path)?;
        }
        // Recreated so a stale socket from a killed daemon cannot linger.
        let daemon = self.daemon_socket();
        if daemon.exists() {
            fs::remove_dir_all(&daemon)?;
        }
        private_directory(&daemon)?;
        fs::write(
            self.resolv_conf(),
            "nameserver 1.1.1.1\nnameserver 1.0.0.1\n",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDirectory;
    use std::time::{Duration, Instant};

    fn temporary_store(label: &str) -> (TempDirectory, Store) {
        let root = TempDirectory::new(label);
        let store = Store {
            state: root.path.join("state"),
            runtime: root.path.join("runtime"),
        };
        (root, store)
    }

    #[test]
    fn digits_select_indices_and_names_start_with_a_letter() {
        assert_eq!("7".parse(), Ok(Selector::Index(7)));
        assert!("256".parse::<Selector>().is_err());
        assert!(matches!("tokyo-2".parse(), Ok(Selector::Name(_))));
        for invalid in ["Tokyo", "-a", "a_b"] {
            assert!(invalid.parse::<Selector>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn names_are_unique_and_follow_renames() {
        let (_root, store) = temporary_store("names");
        let tokyo: Name = "tokyo".parse().unwrap();
        let osaka: Name = "osaka".parse().unwrap();
        store.assign(&store.instance(2), &tokyo).unwrap();
        let selector = Selector::Name(tokyo.clone());
        assert_eq!(store.resolve(&selector).unwrap().index, 2);
        assert!(store.assign(&store.instance(3), &tokyo).is_err());
        store.assign(&store.instance(2), &osaka).unwrap();
        assert!(store.resolve(&selector).is_err());
        store.assign(&store.instance(3), &tokyo).unwrap();
        assert_eq!(store.resolve(&selector).unwrap().index, 3);
    }

    #[test]
    fn locks_exclude_each_other_until_released() {
        let (_root, store) = temporary_store("lock");
        let instance = store.instance(1);
        let lock = instance.lock().unwrap();
        assert!(instance.lock().is_err());
        let descriptor = OwnedFd::from(lock);
        assert!(instance.lock().is_err());
        drop(descriptor);
        // Concurrent test processes can inherit the descriptor between fork and exec.
        let deadline = Instant::now() + Duration::from_secs(1);
        while instance.lock().is_err() {
            assert!(
                Instant::now() < deadline,
                "lock remained held after release"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn recovers_backup_before_discarding_staging() {
        let (_root, store) = temporary_store("recovery");
        for (index, target, expected) in [
            (1, None, "old"),
            (2, Some(false), "old"),
            (3, Some(true), "new"),
        ] {
            let instance = store.instance(index);
            fs::create_dir_all(instance.state().join(".registration-backup")).unwrap();
            fs::write(
                instance.state().join(".registration-backup/reg.json"),
                "old",
            )
            .unwrap();
            fs::create_dir_all(instance.state().join(".registration-import")).unwrap();
            fs::write(
                instance.state().join(".registration-import/reg.json"),
                "partial",
            )
            .unwrap();
            if let Some(valid) = target {
                fs::create_dir_all(instance.registration()).unwrap();
                if valid {
                    fs::write(instance.registration().join("reg.json"), "new").unwrap();
                }
            }
            instance.prepare().unwrap();
            assert_eq!(
                fs::read_to_string(instance.registration().join("reg.json")).unwrap(),
                expected
            );
            assert!(!instance.state().join(".registration-backup").exists());
            assert!(!instance.state().join(".registration-import").exists());
        }
    }

    #[test]
    fn imports_registration_and_finds_team_edge() {
        let (root, store) = temporary_store("import");
        let source = root.path.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("reg.json"), "first").unwrap();
        fs::write(source.join("unrelated"), "not imported").unwrap();
        fs::write(
            source.join("conf.json"),
            r#"{"account":{"account_type":"team"},"endpoints":[{"v4":"192.0.2.4:443"}]}"#,
        )
        .unwrap();
        let instance = store.instance(7);

        instance.import_registration(&source, false).unwrap();
        assert!(!instance.registration().join("unrelated").exists());
        assert_eq!(
            fs::read_to_string(source.join("reg.json")).unwrap(),
            "first"
        );
        assert_eq!(
            instance.registration_edge(443).unwrap(),
            Some(Ipv4Addr::new(192, 0, 2, 4))
        );
        assert_eq!(
            fs::metadata(instance.registration().join("reg.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(instance.import_registration(&source, false).is_err());
        fs::write(source.join("reg.json"), "second").unwrap();
        instance.import_registration(&source, true).unwrap();
        assert_eq!(
            fs::read_to_string(instance.registration().join("reg.json")).unwrap(),
            "second"
        );
        fs::write(source.join("reg.json"), "third").unwrap();
        instance.import_registration(&source, true).unwrap();
        assert_eq!(
            fs::read_to_string(instance.registration().join("reg.json")).unwrap(),
            "third"
        );
    }
}
