use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

const HTTP_KEYS: [&str; 2] = ["HTTP_PROXY", "http_proxy"];
const HTTPS_KEYS: [&str; 2] = ["HTTPS_PROXY", "https_proxy"];
const ALL_KEYS: [&str; 2] = ["ALL_PROXY", "all_proxy"];
const NO_KEYS: [&str; 2] = ["NO_PROXY", "no_proxy"];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SystemProxy {
    pub http: Option<String>,
    pub https: Option<String>,
    pub all: Option<String>,
    pub no_proxy: Option<String>,
}

/// Returns only values wmcterm should add to a child. Existing variables are
/// never copied or changed. Names are compared case-insensitively as proxy
/// variables have historically had both spellings.
pub(crate) fn environment_for_child(
    system: &SystemProxy,
    explicit: &HashMap<String, String>,
) -> Vec<(OsString, OsString)> {
    let inherited = std::env::vars_os().collect::<Vec<_>>();
    merge_environment(&inherited, explicit, system)
}

pub(crate) fn merge_environment(
    inherited: &[(OsString, OsString)],
    explicit: &HashMap<String, String>,
    system: &SystemProxy,
) -> Vec<(OsString, OsString)> {
    let present = |keys: &[&str]| {
        inherited.iter().any(|(key, _)| key_matches(key, keys))
            || explicit
                .keys()
                .any(|key| key_matches(OsStr::new(key), keys))
    };
    let has_all = present(&ALL_KEYS);

    let mut result = Vec::new();
    if !has_all && !present(&HTTP_KEYS) {
        append_pair(&mut result, &HTTP_KEYS, system.http.as_deref());
    }
    if !has_all && !present(&HTTPS_KEYS) {
        append_pair(&mut result, &HTTPS_KEYS, system.https.as_deref());
    }
    if !has_all {
        append_pair(&mut result, &ALL_KEYS, system.all.as_deref());
    }
    if !present(&NO_KEYS) {
        append_pair(&mut result, &NO_KEYS, system.no_proxy.as_deref());
    }
    result
}

fn key_matches(key: &OsStr, candidates: &[&str]) -> bool {
    key.to_str().is_some_and(|key| {
        candidates
            .iter()
            .any(|candidate| key.eq_ignore_ascii_case(candidate))
    })
}

fn append_pair(result: &mut Vec<(OsString, OsString)>, keys: &[&str; 2], value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        for key in keys {
            result.push(((*key).into(), value.into()));
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn detect() -> Result<SystemProxy, ()> {
    linux::read()
}

#[cfg(target_os = "windows")]
pub(crate) fn detect() -> Result<SystemProxy, ()> {
    windows::read()
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(crate) fn detect() -> Result<SystemProxy, ()> {
    Ok(SystemProxy::default())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::SystemProxy;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    pub(super) fn read() -> Result<SystemProxy, ()> {
        if std::env::var("XDG_CURRENT_DESKTOP")
            .is_ok_and(|desktop| desktop.to_ascii_lowercase().contains("kde"))
        {
            return read_kde();
        }
        read_gnome().or_else(|_| read_kde())
    }

    fn read_gnome() -> Result<SystemProxy, ()> {
        let output = Command::new("gsettings")
            .args(["list-recursively", "org.gnome.system.proxy"])
            .output()
            .map_err(|_| ())?;
        if !output.status.success() {
            return Err(());
        }
        let output = String::from_utf8(output.stdout).map_err(|_| ())?;
        parse_gnome_settings(&output).ok_or(())
    }

    fn parse_gnome_settings(output: &str) -> Option<SystemProxy> {
        let values = output
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(3, char::is_whitespace);
                let schema = fields.next()?;
                let key = fields.next()?;
                let value = fields.next()?.trim();
                Some((format!("{schema}.{key}"), value))
            })
            .collect::<HashMap<_, _>>();
        let mode = values.get("org.gnome.system.proxy.mode")?;
        // `auto` is PAC/WPAD and cannot be represented by static proxy env vars.
        if unquote(mode) != "manual" {
            return Some(SystemProxy::default());
        }
        let use_same = values
            .get("org.gnome.system.proxy.use-same-proxy")
            .is_some_and(|value| *value == "true");
        let http = endpoint(&values, "http");
        let https = if use_same {
            http.clone()
        } else {
            endpoint(&values, "https")
        };
        let all = endpoint(&values, "socks");
        let no_proxy = values
            .get("org.gnome.system.proxy.ignore-hosts")
            .and_then(|value| parse_string_array(value));
        Some(SystemProxy {
            http,
            https,
            all,
            no_proxy,
        })
    }

    fn read_kde() -> Result<SystemProxy, ()> {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or(())?;
        let contents = fs::read_to_string(config_home.join("kioslaverc")).map_err(|_| ())?;
        Ok(parse_kioslaverc(&contents).unwrap_or_default())
    }

    fn parse_kioslaverc(contents: &str) -> Option<SystemProxy> {
        let mut in_proxy_settings = false;
        let mut values = HashMap::new();
        for raw_line in contents.lines() {
            let line = raw_line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                in_proxy_settings = line.eq_ignore_ascii_case("[Proxy Settings]");
            } else if in_proxy_settings
                && !line.starts_with('#')
                && let Some((key, value)) = line.split_once('=')
            {
                values.insert(key.trim().to_ascii_lowercase(), value.trim());
            }
        }
        // KDE: 0=direct, 1=manual, 2=PAC, 3=WPAD, 4=environment.
        // Only manual settings have a lossless static environment equivalent.
        if values.get("proxytype").copied() != Some("1") {
            return None;
        }
        Some(SystemProxy {
            http: values
                .get("httpproxy")
                .and_then(|value| kde_endpoint("http", value)),
            https: values
                .get("httpsproxy")
                .and_then(|value| kde_endpoint("http", value)),
            all: values
                .get("socksproxy")
                .and_then(|value| kde_endpoint("socks", value)),
            no_proxy: values
                .get("noproxyfor")
                .map(|value| value.replace(';', ","))
                .filter(|value| !value.is_empty()),
        })
    }

    fn kde_endpoint(scheme: &str, value: &str) -> Option<String> {
        let value = value.trim();
        if value.is_empty() || value.contains('@') {
            return None;
        }
        // Some KDE releases serialize the port after a space.
        let value = value
            .split_once(' ')
            .map_or_else(|| value.to_owned(), |(host, port)| format!("{host}:{port}"));
        if value.contains("://") {
            Some(value)
        } else {
            Some(format!("{scheme}://{value}"))
        }
    }

    fn endpoint(values: &HashMap<String, &str>, scheme: &str) -> Option<String> {
        let host = values
            .get(&format!("org.gnome.system.proxy.{scheme}.host"))
            .map(|value| unquote(value))?;
        let port = values
            .get(&format!("org.gnome.system.proxy.{scheme}.port"))?
            .trim()
            .parse::<u16>()
            .ok()?;
        if host.is_empty() || port == 0 {
            return None;
        }
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host
        };
        let proxy_scheme = if scheme == "socks" { "socks" } else { "http" };
        Some(format!("{proxy_scheme}://{host}:{port}"))
    }

    fn unquote(value: &str) -> String {
        value.trim().trim_matches('\'').replace("\\'", "'")
    }

    fn parse_string_array(value: &str) -> Option<String> {
        let values = value
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(unquote)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join(","))
    }

    #[test]
    fn parses_gsettings_ignore_hosts() {
        assert_eq!(
            parse_string_array("['localhost', '127.0.0.0/8']").as_deref(),
            Some("localhost,127.0.0.0/8")
        );
        assert_eq!(parse_string_array("[]"), None);
    }

    #[test]
    fn parses_single_gsettings_recursive_snapshot_with_child_schemas() {
        let proxy = parse_gnome_settings(
            "org.gnome.system.proxy mode 'manual'\n\
             org.gnome.system.proxy use-same-proxy false\n\
             org.gnome.system.proxy ignore-hosts ['localhost', '*.internal']\n\
             org.gnome.system.proxy.http host '2001:db8::1'\n\
             org.gnome.system.proxy.http port 8080\n\
             org.gnome.system.proxy.https host 'secure.local'\n\
             org.gnome.system.proxy.https port 8443\n\
             org.gnome.system.proxy.socks host ''\n\
             org.gnome.system.proxy.socks port 0\n",
        )
        .unwrap();
        assert_eq!(proxy.http.as_deref(), Some("http://[2001:db8::1]:8080"));
        assert_eq!(proxy.https.as_deref(), Some("http://secure.local:8443"));
        assert_eq!(proxy.all, None);
        assert_eq!(proxy.no_proxy.as_deref(), Some("localhost,*.internal"));

        assert_eq!(
            parse_gnome_settings("org.gnome.system.proxy mode 'auto'").unwrap(),
            SystemProxy::default()
        );
    }

    use std::collections::HashMap;

    #[test]
    fn parses_only_manual_kde_proxy_settings() {
        let proxy = parse_kioslaverc(
            "[Proxy Settings]\nProxyType=1\nhttpProxy=proxy.local 8080\nhttpsProxy=https://secure.local:8443\nsocksProxy=socks.local:1080\nNoProxyFor=localhost;*.internal\n",
        )
        .unwrap();
        assert_eq!(proxy.http.as_deref(), Some("http://proxy.local:8080"));
        assert_eq!(proxy.https.as_deref(), Some("https://secure.local:8443"));
        assert_eq!(proxy.all.as_deref(), Some("socks://socks.local:1080"));
        assert_eq!(proxy.no_proxy.as_deref(), Some("localhost,*.internal"));
        assert!(
            parse_kioslaverc(
                "[Proxy Settings]\nProxyType=2\nProxy Config Script=http://pac/proxy.pac"
            )
            .is_none()
        );
        assert!(parse_kioslaverc("[Proxy Settings]\nProxyType=4\n").is_none());
    }

    #[test]
    fn rejects_kde_proxy_endpoint_with_embedded_credentials() {
        assert_eq!(kde_endpoint("http", "http://user:secret@proxy:80"), None);
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::SystemProxy;
    use std::ptr;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_QUERY_VALUE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey,
        RegGetValueW, RegOpenKeyExW,
    };

    pub(super) fn read() -> Result<SystemProxy, ()> {
        let path = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
        let mut key = ptr::null_mut();
        if unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut key,
            )
        } != 0
        {
            return Err(());
        }
        let enabled = read_dword(key, "ProxyEnable") == Some(1);
        let server = enabled.then(|| read_string(key, "ProxyServer")).flatten();
        let bypass = read_string(key, "ProxyOverride");
        unsafe { RegCloseKey(key) };
        Ok(server
            .map(|value| parse_server(&value, bypass.as_deref()))
            .unwrap_or_default())
    }

    fn parse_server(value: &str, bypass: Option<&str>) -> SystemProxy {
        let mut result = SystemProxy::default();
        if value.contains('=') {
            for item in value.split(';') {
                let Some((kind, endpoint)) = item.split_once('=') else {
                    continue;
                };
                match kind.to_ascii_lowercase().as_str() {
                    "http" => result.http = normalize("http", endpoint),
                    "https" => result.https = normalize("http", endpoint),
                    "socks" | "socks5" => result.all = normalize("socks", endpoint),
                    _ => {}
                }
            }
        } else {
            result.http = normalize("http", value);
            result.https = normalize("http", value);
        }
        result.no_proxy = bypass.map(|value| {
            value
                .replace(';', ",")
                .replace("<local>", "localhost,127.0.0.1,::1")
        });
        result
    }

    fn normalize(scheme: &str, value: &str) -> Option<String> {
        let value = value.trim();
        if value.is_empty() {
            None
        } else if value.contains("://") {
            Some(value.into())
        } else {
            Some(format!("{scheme}://{value}"))
        }
    }
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    fn read_dword(key: *mut core::ffi::c_void, name: &str) -> Option<u32> {
        let name = wide(name);
        let mut value = 0u32;
        let mut size = 4u32;
        (unsafe {
            RegGetValueW(
                key,
                ptr::null(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut size,
            )
        } == 0)
            .then_some(value)
    }
    fn read_string(key: *mut core::ffi::c_void, name: &str) -> Option<String> {
        let name = wide(name);
        let mut size = 0u32;
        if unsafe {
            RegGetValueW(
                key,
                ptr::null(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut size,
            )
        } != 0
        {
            return None;
        }
        let mut value = vec![0u16; size as usize / 2];
        if unsafe {
            RegGetValueW(
                key,
                ptr::null(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                ptr::null_mut(),
                value.as_mut_ptr().cast(),
                &mut size,
            )
        } != 0
        {
            return None;
        }
        value.last().is_some_and(|v| *v == 0).then(|| {
            value.pop();
        });
        String::from_utf16(&value).ok()
    }

    #[test]
    fn parses_wininet_proxy_server() {
        let proxy = parse_server(
            "http=proxy:80;https=secure:443;socks=local:1080",
            Some("localhost;<local>"),
        );
        assert_eq!(proxy.http.as_deref(), Some("http://proxy:80"));
        assert_eq!(proxy.https.as_deref(), Some("http://secure:443"));
        assert_eq!(proxy.all.as_deref(), Some("socks://local:1080"));
    }

    #[test]
    fn parses_wininet_single_proxy_and_ignores_invalid_segments() {
        let single = parse_server("proxy.local:8080", None);
        assert_eq!(single.http.as_deref(), Some("http://proxy.local:8080"));
        assert_eq!(single.https.as_deref(), Some("http://proxy.local:8080"));

        let segmented = parse_server("broken;ftp=ignored:21;https=secure:443", None);
        assert_eq!(segmented.http, None);
        assert_eq!(segmented.https.as_deref(), Some("http://secure:443"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_protocol_wins_and_system_fills_missing_protocol() {
        let inherited = vec![("http_proxy".into(), "http://user".into())];
        let explicit = HashMap::new();
        let system = SystemProxy {
            http: Some("http://desktop".into()),
            https: Some("http://desktop".into()),
            no_proxy: Some("localhost".into()),
            ..Default::default()
        };
        let result = merge_environment(&inherited, &explicit, &system);
        assert!(!result.iter().any(|(key, _)| key_matches(key, &HTTP_KEYS)));
        assert!(result.iter().any(|(key, _)| key == "HTTPS_PROXY"));
    }

    #[test]
    fn all_proxy_prevents_proxy_injection_but_not_independent_bypass() {
        let inherited = vec![("All_Proxy".into(), "socks5://user".into())];
        let result = merge_environment(
            &inherited,
            &HashMap::new(),
            &SystemProxy {
                http: Some("http://desktop".into()),
                no_proxy: Some("localhost".into()),
                ..Default::default()
            },
        );
        assert!(!result.iter().any(|(key, _)| key_matches(key, &HTTP_KEYS)));
        assert!(result.iter().any(|(key, _)| key == "NO_PROXY"));
    }

    #[test]
    fn no_proxy_is_preserved_instead_of_merged() {
        let inherited = vec![("NO_PROXY".into(), "internal".into())];
        let result = merge_environment(
            &inherited,
            &HashMap::new(),
            &SystemProxy {
                no_proxy: Some("localhost".into()),
                ..Default::default()
            },
        );
        assert!(result.is_empty());
    }

    #[test]
    fn explicit_environment_wins_when_inherited_is_empty() {
        let explicit = HashMap::from([("Https_Proxy".into(), "http://rpc".into())]);
        let result = merge_environment(
            &[],
            &explicit,
            &SystemProxy {
                https: Some("http://desktop".into()),
                ..Default::default()
            },
        );
        assert!(result.is_empty());
    }

    #[test]
    fn empty_system_values_are_not_injected() {
        let system = SystemProxy {
            http: Some(String::new()),
            ..Default::default()
        };
        assert!(merge_environment(&[], &HashMap::new(), &system).is_empty());
    }
}
