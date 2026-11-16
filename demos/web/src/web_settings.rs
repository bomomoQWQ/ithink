//! Host-only provider settings. Keys live in memory and never in the public view.
use ithink::KernelError;
use ithink::openai::{ProviderConfig, ReasoningEffort, ResponseFormat, ThinkingMode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_SAVED_BYTES: usize = 131_072;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct Settings {
    pub mode: String,
    pub provider: ProviderConfig,
    pub path: Option<PathBuf>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    pub mode: String,
    pub endpoint: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub clear_api_key: bool,
    pub system_prompt: String,
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stop: Vec<String>,
    #[serde(default)]
    pub response_format: ResponseFormat,
    #[serde(default)]
    pub thinking: ThinkingMode,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub safety_check: bool,
    pub timeout_secs: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    mode: String,
    endpoint: String,
    model: String,
    system_prompt: String,
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    stop: Vec<String>,
    #[serde(default)]
    response_format: ResponseFormat,
    #[serde(default)]
    thinking: ThinkingMode,
    #[serde(default)]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    safety_check: bool,
    timeout_secs: u64,
}
impl Settings {
    pub fn load(directory: Option<&Path>, offline: bool) -> Result<Self, KernelError> {
        let path = directory.map(|p| p.join("provider.json"));
        let mut settings = Self {
            mode: if offline { "offline" } else { "api" }.into(),
            provider: ProviderConfig::default(),
            path,
        };
        if let Some(path) = &settings.path {
            match read_saved(path) {
                Ok(Some(value)) => {
                    let mode = if offline {
                        "offline".to_string()
                    } else {
                        value.mode
                    };
                    let provider = ProviderConfig {
                        endpoint: value.endpoint,
                        model: value.model,
                        api_key: String::new(),
                        system_prompt: value.system_prompt,
                        temperature: value.temperature,
                        top_p: value.top_p,
                        max_tokens: value.max_tokens,
                        stop: value.stop,
                        response_format: value.response_format,
                        thinking: value.thinking,
                        reasoning_effort: value.reasoning_effort,
                        safety_check: value.safety_check,
                        timeout_secs: value.timeout_secs,
                    };
                    if matches!(mode.as_str(), "api" | "offline") && provider.validate().is_ok() {
                        settings.mode = mode;
                        settings.provider = provider;
                    } else {
                        warn_invalid_saved();
                    }
                }
                Ok(None) => {}
                Err(_) => warn_invalid_saved(),
            }
        }
        Ok(settings)
    }
    pub fn updated(&self, input: SettingsUpdate) -> Result<Self, KernelError> {
        if !matches!(input.mode.as_str(), "api" | "offline") {
            return Err(KernelError::invalid("不支持的模型模式"));
        }
        let endpoint = input.endpoint.trim().to_string();
        // A blank field means "keep" only for the same normalized security
        // origin. Changing scheme/host/effective port must not forward an old
        // provider's Authorization to a newly selected server or connection test.
        let same_origin = endpoint_origin(&self.provider.endpoint)
            .zip(endpoint_origin(&endpoint))
            .is_some_and(|(old, new)| old == new);
        let mut provider = ProviderConfig {
            endpoint,
            model: input.model.trim().into(),
            api_key: if same_origin {
                self.provider.api_key.clone()
            } else {
                String::new()
            },
            system_prompt: input.system_prompt,
            temperature: input.temperature,
            top_p: input.top_p,
            max_tokens: input.max_tokens,
            stop: input.stop,
            response_format: input.response_format,
            thinking: input.thinking,
            reasoning_effort: input.reasoning_effort,
            safety_check: input.safety_check,
            timeout_secs: input.timeout_secs,
        };
        if input.clear_api_key {
            provider.api_key.clear();
        }
        if let Some(key) = input.api_key.filter(|k| !k.is_empty()) {
            provider.api_key = key;
        }
        provider.validate()?;
        Ok(Self {
            mode: input.mode,
            provider,
            path: self.path.clone(),
        })
    }
    pub fn configured(&self) -> bool {
        let local = reqwest::Url::parse(&self.provider.endpoint)
            .ok()
            .is_some_and(|url| {
                url.host_str().is_some_and(|host| {
                    host.eq_ignore_ascii_case("localhost")
                        || host.trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| {
                                ip.is_loopback() || matches!(ip, std::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()))
                            })
                })
            });
        self.mode == "offline" || !self.provider.api_key.is_empty() || local
    }
    pub fn public(&self) -> Value {
        json!({"mode":self.mode,"endpoint":self.provider.endpoint,"model":self.provider.model,"api_key_set":!self.provider.api_key.is_empty(),"system_prompt":self.provider.system_prompt,"temperature":self.provider.temperature,"top_p":self.provider.top_p,"max_tokens":self.provider.max_tokens,"stop":self.provider.stop,"response_format":self.provider.response_format,"thinking":self.provider.thinking,"reasoning_effort":self.provider.reasoning_effort,"safety_check":self.provider.safety_check,"timeout_secs":self.provider.timeout_secs,"configured":self.configured()})
    }
    pub fn persist(&self) -> Result<(), KernelError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path
            .parent()
            .ok_or_else(|| KernelError::new("settings_io", "设置目录无效"))?;
        std::fs::create_dir_all(parent)
            .map_err(|_| KernelError::new("settings_io", "无法创建设置目录"))?;
        let saved = Saved {
            mode: self.mode.clone(),
            endpoint: self.provider.endpoint.clone(),
            model: self.provider.model.clone(),
            system_prompt: self.provider.system_prompt.clone(),
            temperature: self.provider.temperature,
            top_p: self.provider.top_p,
            max_tokens: self.provider.max_tokens,
            stop: self.provider.stop.clone(),
            response_format: self.provider.response_format,
            thinking: self.provider.thinking,
            reasoning_effort: self.provider.reasoning_effort,
            safety_check: self.provider.safety_check,
            timeout_secs: self.provider.timeout_secs,
        };
        let bytes = serde_json::to_vec_pretty(&saved)
            .map_err(|_| KernelError::new("settings_io", "无法编码设置"))?;
        if bytes.len() > MAX_SAVED_BYTES {
            return Err(KernelError::new("settings_io", "模型设置文件过大"));
        }
        let temporary = path.with_file_name(format!(
            ".provider-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let commit = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| KernelError::new("settings_io", "无法写入设置"))?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| KernelError::new("settings_io", "无法写入设置"))?;
            drop(file);
            atomic_replace(&temporary, path)
                .map_err(|_| KernelError::new("settings_io", "无法保存设置"))?;
            Ok(())
        })();
        // No target truncation occurs before atomic replacement. Cleanup is
        // best effort and must never turn an already committed save into error.
        let _ = std::fs::remove_file(&temporary);
        commit
    }
}

fn endpoint_origin(endpoint: &str) -> Option<(String, String, u16)> {
    let url = reqwest::Url::parse(endpoint).ok()?;
    Some((
        url.scheme().to_ascii_lowercase(),
        url.host_str()?.to_ascii_lowercase(),
        url.port_or_known_default()?,
    ))
}
fn read_saved(path: &Path) -> Result<Option<Saved>, KernelError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(KernelError::new("settings_io", "无法读取模型设置")),
    };
    let mut bytes = vec![];
    file.take((MAX_SAVED_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| KernelError::new("settings_io", "无法读取模型设置"))?;
    if bytes.len() > MAX_SAVED_BYTES {
        return Err(KernelError::new("settings_io", "模型设置文件过大"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| KernelError::new("settings_io", "模型设置格式错误"))
}
fn warn_invalid_saved() {
    // Never print file contents, config values, credential fields or parser text.
    eprintln!("模型设置无法恢复，已使用默认未配置设置；原文件未修改。");
}
#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both paths are NUL-terminated owned buffers kept alive for the
    // entire call. Source is a same-directory file written and synced above.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    const SECRET: &str = "secret-do-not-copy-to-another-origin";
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ithink-settings-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
            assert!(
                self.0
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("ithink-settings-")
            );
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn settings(endpoint: &str) -> Settings {
        Settings {
            mode: "api".into(),
            provider: ProviderConfig {
                endpoint: endpoint.into(),
                api_key: SECRET.into(),
                ..ProviderConfig::default()
            },
            path: None,
        }
    }
    fn update(endpoint: &str) -> SettingsUpdate {
        SettingsUpdate {
            mode: "api".into(),
            endpoint: endpoint.into(),
            model: "model".into(),
            api_key: None,
            clear_api_key: false,
            system_prompt: "system".into(),
            temperature: None,
            top_p: None,
            max_tokens: Some(128),
            stop: vec![],
            response_format: ResponseFormat::Text,
            thinking: ThinkingMode::Auto,
            reasoning_effort: None,
            safety_check: false,
            timeout_secs: 5,
        }
    }
    #[test]
    fn blank_key_is_retained_only_for_the_same_normalized_origin() {
        let current = settings("https://API.EXAMPLE/v1");
        for endpoint in [
            "https://api.example:443/v2",
            "https://api.example/v1/chat/completions",
        ] {
            assert_eq!(
                current.updated(update(endpoint)).unwrap().provider.api_key,
                SECRET
            );
        }
        for endpoint in [
            "https://other.example/v1",
            "https://api.example:444/v1",
            "http://127.0.0.1:9000/v1",
        ] {
            let candidate = current.updated(update(endpoint)).unwrap();
            assert!(candidate.provider.api_key.is_empty());
            assert!(!candidate.public().to_string().contains(SECRET));
            assert_eq!(candidate.public()["api_key_set"], false);
        }
        assert!(
            !current
                .updated(update("https://other.example/v1"))
                .unwrap()
                .configured()
        );
        assert!(
            current
                .updated(update("http://127.0.0.1:9000/v1"))
                .unwrap()
                .configured()
        );
    }
    #[test]
    fn explicit_key_clear_and_new_port_semantics_are_preserved() {
        let current = settings("http://127.0.0.1:8000/v1");
        let mut blank = update("http://127.0.0.1:8000/v2");
        blank.api_key = Some(String::new());
        assert_eq!(current.updated(blank).unwrap().provider.api_key, SECRET);
        assert!(
            current
                .updated(update("http://127.0.0.1:8001/v1"))
                .unwrap()
                .provider
                .api_key
                .is_empty()
        );
        let mut clear = update("http://127.0.0.1:8000/v1");
        clear.clear_api_key = true;
        assert!(current.updated(clear).unwrap().provider.api_key.is_empty());
        let mut replace = update("https://new.example/v1");
        replace.api_key = Some("new-key".into());
        assert_eq!(
            current.updated(replace).unwrap().provider.api_key,
            "new-key"
        );
    }
    #[test]
    fn changing_mock_endpoint_does_not_forward_old_authorization_or_expose_key() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::time::Duration;
        fn mock() -> (String, mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
            let (tx, rx) = mpsc::sync_channel(1);
            let worker = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse().unwrap())
                    })
                    .unwrap();
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                tx.send(headers).unwrap();
                let body=b"{\"choices\":[{\"finish_reason\":\"stop\",\"message\":{\"role\":\"assistant\",\"content\":\"OK\"}}]}";
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                socket.write_all(body).unwrap();
            });
            (endpoint, rx, worker)
        }
        let (first, first_rx, first_worker) = mock();
        let current = settings(&first);
        ithink::openai::test_connection(&current.provider).unwrap();
        assert!(
            first_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {SECRET}"))
        );
        first_worker.join().unwrap();
        let (second, second_rx, second_worker) = mock();
        let candidate = current.updated(update(&second)).unwrap();
        assert!(candidate.configured());
        assert_eq!(candidate.public()["api_key_set"], false);
        assert!(!candidate.public().to_string().contains(SECRET));
        ithink::openai::test_connection(&candidate.provider).unwrap();
        assert!(
            !second_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .to_ascii_lowercase()
                .contains("authorization:")
        );
        second_worker.join().unwrap();
    }
    #[test]
    fn same_origin_replacement_and_restore_never_persist_or_return_key() {
        let temp = Temp::new();
        let path = temp.0.join("provider.json");
        let mut current = settings("https://api.example/v1");
        current.path = Some(path.clone());
        current.persist().unwrap();
        current.provider.model = "second-model".into();
        current.persist().unwrap();
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(!disk.contains(SECRET));
        assert!(!disk.contains("api_key"));
        assert!(!current.public().to_string().contains(SECRET));
        let restored = Settings::load(Some(&temp.0), false).unwrap();
        assert_eq!(restored.provider.model, "second-model");
        assert!(restored.provider.api_key.is_empty());
        assert!(!restored.configured());
        assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 1);
    }
    #[test]
    fn deepseek_parameters_round_trip_without_persisting_the_key() {
        let temp = Temp::new();
        let path = temp.0.join("provider.json");
        let mut current = settings("https://api.deepseek.com");
        current.path = Some(path.clone());
        current.provider.model = "deepseek-flash".into();
        current.provider.thinking = ThinkingMode::Enabled;
        current.provider.reasoning_effort = Some(ReasoningEffort::High);
        current.provider.top_p = Some(0.95);
        current.provider.max_tokens = Some(65_536);
        current.provider.stop = vec!["END".into(), "STOP".into()];
        current.provider.response_format = ResponseFormat::JsonObject;
        current.provider.safety_check = true;
        current.persist().unwrap();

        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(!disk.contains(SECRET));
        assert!(!disk.contains("api_key"));
        let restored = Settings::load(Some(&temp.0), false).unwrap();
        assert_eq!(restored.provider.model, "deepseek-flash");
        assert_eq!(restored.provider.thinking, ThinkingMode::Enabled);
        assert_eq!(
            restored.provider.reasoning_effort,
            Some(ReasoningEffort::High)
        );
        assert_eq!(restored.provider.top_p, Some(0.95));
        assert_eq!(restored.provider.max_tokens, Some(65_536));
        assert_eq!(
            restored.provider.stop,
            vec!["END".to_string(), "STOP".to_string()]
        );
        assert_eq!(
            restored.provider.response_format,
            ResponseFormat::JsonObject
        );
        assert!(restored.provider.safety_check);
        let public = restored.public();
        assert_eq!(public["thinking"], "enabled");
        assert_eq!(public["reasoning_effort"], "high");
        assert_eq!(public["top_p"], 0.95);
        assert_eq!(public["max_tokens"], 65_536);
        assert_eq!(public["response_format"], "json_object");
        assert_eq!(public["safety_check"], true);
        assert!(!public.to_string().contains(SECRET));
    }
    #[test]
    fn legacy_saved_file_without_new_parameters_uses_defaults() {
        let temp = Temp::new();
        let path = temp.0.join("provider.json");
        let legacy = json!({
            "mode": "api",
            "endpoint": "https://api.example/v1",
            "model": "legacy-model",
            "system_prompt": "system",
            "temperature": null,
            "max_tokens": 4096,
            "timeout_secs": 30
        });
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let loaded = Settings::load(Some(&temp.0), false).unwrap();
        assert_eq!(loaded.provider.model, "legacy-model");
        assert_eq!(loaded.provider.max_tokens, Some(4096));
        assert_eq!(loaded.provider.thinking, ThinkingMode::Auto);
        assert_eq!(loaded.provider.reasoning_effort, None);
        assert_eq!(loaded.provider.top_p, None);
        assert_eq!(loaded.provider.response_format, ResponseFormat::Text);
        assert!(loaded.provider.stop.is_empty());
        assert!(!loaded.provider.safety_check);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn invalid_deepseek_parameters_fail_before_save() {
        let current = settings("https://api.deepseek.com");

        let mut disabled_effort = update("https://api.deepseek.com");
        disabled_effort.thinking = ThinkingMode::Disabled;
        disabled_effort.reasoning_effort = Some(ReasoningEffort::High);
        assert_eq!(
            current.updated(disabled_effort).err().unwrap().code,
            "invalid_provider_config"
        );

        let mut bad_top_p = update("https://api.deepseek.com");
        bad_top_p.top_p = Some(0.0);
        assert_eq!(
            current.updated(bad_top_p).err().unwrap().code,
            "invalid_provider_config"
        );

        let mut bad_max_tokens = update("https://api.deepseek.com");
        bad_max_tokens.max_tokens = Some(393_217);
        assert_eq!(
            current.updated(bad_max_tokens).err().unwrap().code,
            "invalid_provider_config"
        );

        let mut bad_stop = update("https://api.deepseek.com");
        bad_stop.stop = vec!["x".into(); 17];
        assert_eq!(
            current.updated(bad_stop).err().unwrap().code,
            "invalid_provider_config"
        );

        let mut omitted_max_tokens = update("https://api.deepseek.com");
        omitted_max_tokens.max_tokens = None;
        let saved = current.updated(omitted_max_tokens).unwrap();
        assert_eq!(saved.provider.max_tokens, None);
        assert_eq!(saved.public()["max_tokens"], Value::Null);
    }
    #[test]
    fn corrupt_saved_settings_do_not_block_start_or_overwrite_evidence() {
        let temp = Temp::new();
        let path = temp.0.join("provider.json");
        for bytes in [
            b"{broken".to_vec(),
            vec![b'x'; MAX_SAVED_BYTES + 1],
            b"{\"mode\":\"api\",\"api_key\":\"sensitive\"}".to_vec(),
        ] {
            std::fs::write(&path, &bytes).unwrap();
            let loaded = Settings::load(Some(&temp.0), false).unwrap();
            assert_eq!(loaded.mode, "api");
            assert!(!loaded.configured());
            assert!(loaded.provider.api_key.is_empty());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }
    #[test]
    fn offline_override_remains_explicit_and_invalid_provider_falls_back() {
        let temp = Temp::new();
        let path = temp.0.join("provider.json");
        let mut current = settings("https://api.example/v1");
        current.path = Some(path.clone());
        current.persist().unwrap();
        assert_eq!(Settings::load(Some(&temp.0), true).unwrap().mode, "offline");
        let mut data: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        data["endpoint"] = json!("http://remote.example/v1");
        let bytes = serde_json::to_vec(&data).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let loaded = Settings::load(Some(&temp.0), false).unwrap();
        assert!(!loaded.configured());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn failed_atomic_replace_does_not_modify_target_or_leave_temporary_payload() {
        let temp = Temp::new();
        let path = temp.0.join("provider.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("marker"), b"leave unchanged").unwrap();
        let mut current = settings("https://api.example/v1");
        current.path = Some(path.clone());
        assert!(current.persist().is_err());
        assert_eq!(
            std::fs::read(path.join("marker")).unwrap(),
            b"leave unchanged"
        );
        assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 1);
    }
}
