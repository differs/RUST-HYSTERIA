use anyhow::{Result, anyhow};

use crate::FormState;

#[cfg(target_os = "android")]
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VpnState {
    pub available: bool,
    pub permission_granted: bool,
    pub active: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchConfig {
    pub server: Option<String>,
    pub auth: Option<String>,
    pub obfs_password: Option<String>,
    pub sni: Option<String>,
    pub ca_path: Option<String>,
    pub pin_sha256: Option<String>,
    pub bandwidth_up: Option<String>,
    pub bandwidth_down: Option<String>,
    pub quic_init_stream_receive_window: Option<String>,
    pub quic_max_stream_receive_window: Option<String>,
    pub quic_init_connection_receive_window: Option<String>,
    pub quic_max_connection_receive_window: Option<String>,
    pub quic_max_idle_timeout: Option<String>,
    pub quic_keep_alive_period: Option<String>,
    pub quic_disable_path_mtu_discovery: Option<bool>,
    pub insecure_tls: Option<bool>,
    pub auto_connect: Option<bool>,
    pub auto_request_vpn: Option<bool>,
    pub auto_start_vpn: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaFile {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaCatalog {
    pub directory: String,
    pub files: Vec<CaFile>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportedConfig {
    pub name: String,
    pub content: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportedCa {
    pub name: String,
    pub path: String,
}

#[cfg(target_os = "android")]
mod imp {
    use anyhow::{Context, Result, anyhow};
    use jni::{
        Env, JavaVM, jni_sig, jni_str,
        objects::{Global, JObject, JString, JValue},
        strings::JNIString,
    };
    use std::sync::{Mutex, OnceLock};

    use super::{CaCatalog, CaFile, FormState, ImportedCa, ImportedConfig, LaunchConfig, VpnState};

    static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();
    static MAIN_ACTIVITY: OnceLock<Mutex<Option<Global<JObject<'static>>>>> = OnceLock::new();
    static VPN_SERVICE: OnceLock<Mutex<Option<Global<JObject<'static>>>>> = OnceLock::new();

    fn main_activity_slot() -> &'static Mutex<Option<Global<JObject<'static>>>> {
        MAIN_ACTIVITY.get_or_init(|| Mutex::new(None))
    }

    fn vpn_service_slot() -> &'static Mutex<Option<Global<JObject<'static>>>> {
        VPN_SERVICE.get_or_init(|| Mutex::new(None))
    }

    pub fn cache_java_vm(env: &mut Env<'_>) -> Result<()> {
        let vm = env
            .get_java_vm()
            .context("failed to get JavaVM from JNI env")?;
        let _ = JAVA_VM.set(vm);
        Ok(())
    }

    pub fn cache_main_activity(env: &mut Env<'_>, activity: &JObject<'_>) -> Result<()> {
        cache_java_vm(env)?;
        let global = env
            .new_global_ref(activity)
            .context("failed to create global ref for MainActivity")?;
        *main_activity_slot()
            .lock()
            .expect("main activity mutex poisoned") = Some(global);
        Ok(())
    }

    pub fn cache_vpn_service(env: &mut Env<'_>, service: &JObject<'_>) -> Result<()> {
        cache_java_vm(env)?;
        let global = env
            .new_global_ref(service)
            .context("failed to create global ref for HysteriaVpnService")?;
        *vpn_service_slot()
            .lock()
            .expect("vpn service mutex poisoned") = Some(global);
        Ok(())
    }

    fn with_env<F, T>(f: F) -> Result<T>
    where
        F: FnOnce(&mut Env<'_>) -> Result<T>,
    {
        if let Some(vm) = JAVA_VM.get() {
            return vm
                .attach_current_thread(f)
                .context("failed to attach current thread to cached JVM");
        }

        let ctx = ndk_context::android_context();
        let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
        let _ = JAVA_VM.set(vm);
        JAVA_VM
            .get()
            .context("cached JVM missing after initialization")?
            .attach_current_thread(f)
            .context("failed to attach current thread to fallback JVM")
    }

    fn with_main_activity<F, T>(f: F) -> Result<T>
    where
        F: FnOnce(&mut Env<'_>, &JObject<'static>) -> Result<T>,
    {
        with_env(|env| {
            let guard = main_activity_slot()
                .lock()
                .expect("main activity mutex poisoned");
            let activity = guard
                .as_ref()
                .context("MainActivity bridge is not ready yet")?;
            f(env, activity.as_obj())
        })
    }

    fn with_vpn_service<F, T>(f: F) -> Result<T>
    where
        F: FnOnce(&mut Env<'_>, &JObject<'static>) -> Result<T>,
    {
        with_env(|env| {
            let guard = vpn_service_slot()
                .lock()
                .expect("vpn service mutex poisoned");
            let service = guard
                .as_ref()
                .context("HysteriaVpnService bridge is not ready yet")?;
            f(env, service.as_obj())
        })
    }

    pub fn request_permission() -> Result<()> {
        with_main_activity(|env, activity| {
            env.call_method(
                activity,
                jni_str!("requestVpnPermissionFromRust"),
                jni_sig!("()V"),
                &[],
            )
            .context("failed to request Android VPN permission")?;
            Ok(())
        })
    }

    pub fn start_vpn_service(socks_host: &str, socks_port: i32) -> Result<()> {
        with_main_activity(|env, activity| {
            let host = env
                .new_string(socks_host)
                .context("failed to create VPN socks host string")?;
            let host_obj = JObject::from(host);
            env.call_method(
                activity,
                jni_str!("startVpnServiceFromRust"),
                jni_sig!("(Ljava/lang/String;I)V"),
                &[JValue::Object(&host_obj), JValue::Int(socks_port)],
            )
            .context("failed to start Android VPN service")?;
            Ok(())
        })
    }

    pub fn start_managed_vpn(form: &FormState, socks_host: &str, socks_port: i32) -> Result<()> {
        with_main_activity(|env, activity| {
            let server = env
                .new_string(form.server.as_str())
                .context("failed to create managed VPN server string")?;
            let auth = env
                .new_string(form.auth.as_str())
                .context("failed to create managed VPN auth string")?;
            let obfs_password = env
                .new_string(form.obfs_password.as_str())
                .context("failed to create managed VPN obfs string")?;
            let sni = env
                .new_string(form.sni.as_str())
                .context("failed to create managed VPN SNI string")?;
            let ca_path = env
                .new_string(form.ca_path.as_str())
                .context("failed to create managed VPN CA path string")?;
            let pin_sha256 = env
                .new_string(form.pin_sha256.as_str())
                .context("failed to create managed VPN pin string")?;
            let bandwidth_up = env
                .new_string(form.bandwidth_up.as_str())
                .context("failed to create managed VPN bandwidth up string")?;
            let bandwidth_down = env
                .new_string(form.bandwidth_down.as_str())
                .context("failed to create managed VPN bandwidth down string")?;
            let quic_init_stream_receive_window = env
                .new_string(form.quic_init_stream_receive_window.as_str())
                .context("failed to create managed VPN QUIC init stream window string")?;
            let quic_max_stream_receive_window = env
                .new_string(form.quic_max_stream_receive_window.as_str())
                .context("failed to create managed VPN QUIC max stream window string")?;
            let quic_init_connection_receive_window = env
                .new_string(form.quic_init_connection_receive_window.as_str())
                .context("failed to create managed VPN QUIC init conn window string")?;
            let quic_max_connection_receive_window = env
                .new_string(form.quic_max_connection_receive_window.as_str())
                .context("failed to create managed VPN QUIC max conn window string")?;
            let quic_max_idle_timeout = env
                .new_string(form.quic_max_idle_timeout.as_str())
                .context("failed to create managed VPN QUIC idle timeout string")?;
            let quic_keep_alive_period = env
                .new_string(form.quic_keep_alive_period.as_str())
                .context("failed to create managed VPN QUIC keepalive string")?;
            let host = env
                .new_string(socks_host)
                .context("failed to create managed VPN socks host string")?;

            env.call_method(
                activity,
                jni_str!("startManagedVpnFieldsFromRust"),
                jni_sig!(
                    "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ZZLjava/lang/String;I)V"
                ),
                &[
                    JValue::Object(&JObject::from(server)),
                    JValue::Object(&JObject::from(auth)),
                    JValue::Object(&JObject::from(obfs_password)),
                    JValue::Object(&JObject::from(sni)),
                    JValue::Object(&JObject::from(ca_path)),
                    JValue::Object(&JObject::from(pin_sha256)),
                    JValue::Object(&JObject::from(bandwidth_up)),
                    JValue::Object(&JObject::from(bandwidth_down)),
                    JValue::Object(&JObject::from(quic_init_stream_receive_window)),
                    JValue::Object(&JObject::from(quic_max_stream_receive_window)),
                    JValue::Object(&JObject::from(quic_init_connection_receive_window)),
                    JValue::Object(&JObject::from(quic_max_connection_receive_window)),
                    JValue::Object(&JObject::from(quic_max_idle_timeout)),
                    JValue::Object(&JObject::from(quic_keep_alive_period)),
                    JValue::Bool(form.quic_disable_path_mtu_discovery.into()),
                    JValue::Bool(form.insecure_tls.into()),
                    JValue::Object(&JObject::from(host)),
                    JValue::Int(socks_port),
                ],
            )
            .context("failed to request managed Android VPN start")?;
            Ok(())
        })
    }

    pub fn stop_vpn_service() -> Result<()> {
        if let Ok(result) = with_vpn_service(|env, service| {
            env.call_method(
                service,
                jni_str!("stopManagedFromRust"),
                jni_sig!("()V"),
                &[],
            )
            .context("failed to stop Android VPN service")?;
            Ok(())
        }) {
            return Ok(result);
        }

        with_main_activity(|env, activity| {
            env.call_method(
                activity,
                jni_str!("stopVpnServiceFromRust"),
                jni_sig!("()V"),
                &[],
            )
            .context("failed to stop Android VPN service via MainActivity")?;
            Ok(())
        })
    }

    pub fn take_tun_fd() -> Result<i32> {
        with_vpn_service(|env, service| {
            env.call_method(service, jni_str!("takeTunFdFromRust"), jni_sig!("()I"), &[])
                .context("failed to take Android VPN TUN fd")?
                .i()
                .context("invalid Android VPN TUN fd")
        })
    }

    pub fn query_state() -> Result<VpnState> {
        if let Ok(state) = with_vpn_service(|env, service| {
            let permission_granted = env
                .call_method(
                    service,
                    jni_str!("isPermissionGrantedFromRust"),
                    jni_sig!("()Z"),
                    &[],
                )
                .context("failed to query Android VPN permission state")?
                .z()
                .context("invalid VPN permission state")?;
            let active = env
                .call_method(service, jni_str!("isActiveFromRust"), jni_sig!("()Z"), &[])
                .context("failed to query Android VPN active state")?
                .z()
                .context("invalid VPN active state")?;
            Ok(VpnState {
                available: true,
                permission_granted,
                active,
            })
        }) {
            return Ok(state);
        }

        with_main_activity(|env, activity| {
            let permission_granted = env
                .call_method(
                    activity,
                    jni_str!("isVpnPermissionGrantedFromRust"),
                    jni_sig!("()Z"),
                    &[],
                )
                .context("failed to query Android VPN permission state from MainActivity")?
                .z()
                .context("invalid VPN permission state from MainActivity")?;
            let active = env
                .call_method(
                    activity,
                    jni_str!("isVpnActiveFromRust"),
                    jni_sig!("()Z"),
                    &[],
                )
                .context("failed to query Android VPN active state from MainActivity")?
                .z()
                .context("invalid VPN active state from MainActivity")?;
            Ok(VpnState {
                available: true,
                permission_granted,
                active,
            })
        })
    }

    fn launch_string_extra(
        env: &mut Env<'_>,
        activity: &JObject<'static>,
        key: &str,
    ) -> Result<Option<String>> {
        let key = env
            .new_string(key)
            .context("failed to create Android launch extra key")?;
        let key_obj = JObject::from(key);
        let value = env
            .call_method(
                activity,
                jni_str!("launchStringExtraFromRust"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
                &[JValue::Object(&key_obj)],
            )
            .context("failed to query Android string launch extra")?
            .l()
            .context("invalid Android string launch extra")?;
        if value.is_null() {
            Ok(None)
        } else {
            let value = unsafe { JString::from_raw(env, value.into_raw().cast()) };
            Ok(Some(value.to_string()))
        }
    }

    fn launch_bool_extra(
        env: &mut Env<'_>,
        activity: &JObject<'static>,
        key: &str,
    ) -> Result<bool> {
        let key = env
            .new_string(key)
            .context("failed to create Android launch bool key")?;
        let key_obj = JObject::from(key);
        env.call_method(
            activity,
            jni_str!("launchBooleanExtraFromRust"),
            jni_sig!("(Ljava/lang/String;)Z"),
            &[JValue::Object(&key_obj)],
        )
        .context("failed to query Android bool launch extra")?
        .z()
        .context("invalid Android bool launch extra")
    }

    fn activity_string_method(
        env: &mut Env<'_>,
        activity: &JObject<'static>,
        method: &'static str,
    ) -> Result<String> {
        let value = env
            .call_method(
                activity,
                JNIString::from(method),
                jni_sig!("()Ljava/lang/String;"),
                &[],
            )
            .with_context(|| format!("failed to call Android bridge method {method}"))?
            .l()
            .with_context(|| format!("invalid Android bridge string return from {method}"))?;
        if value.is_null() {
            return Ok(String::new());
        }
        let value = unsafe { JString::from_raw(env, value.into_raw().cast()) };
        Ok(value.to_string())
    }

    fn activity_void_method(
        env: &mut Env<'_>,
        activity: &JObject<'static>,
        method: &'static str,
    ) -> Result<()> {
        env.call_method(activity, JNIString::from(method), jni_sig!("()V"), &[])
            .with_context(|| format!("failed to call Android bridge method {method}"))?;
        Ok(())
    }

    fn parse_ca_listing(listing: &str) -> Vec<CaFile> {
        listing
            .lines()
            .filter_map(|line| {
                let (name, path) = line.split_once('\t')?;
                let name = name.trim();
                let path = path.trim();
                if name.is_empty() || path.is_empty() {
                    return None;
                }
                Some(CaFile {
                    name: name.to_string(),
                    path: path.to_string(),
                })
            })
            .collect()
    }

    fn parse_imported_config_payload(payload: &str) -> Result<Option<ImportedConfig>> {
        if payload.trim().is_empty() {
            return Ok(None);
        }
        let mut parts = payload.splitn(3, '\u{1f}');
        match parts.next() {
            Some("ok") => {
                let name = parts.next().unwrap_or_default().trim().to_string();
                let content = parts.next().unwrap_or_default().to_string();
                if name.is_empty() && content.trim().is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(ImportedConfig { name, content }))
                }
            }
            Some("err") => Err(anyhow!(
                parts
                    .next()
                    .unwrap_or("Android config import failed")
                    .to_string()
            )),
            _ => Err(anyhow!("invalid Android config import payload")),
        }
    }

    fn parse_imported_ca_payload(payload: &str) -> Result<Option<ImportedCa>> {
        if payload.trim().is_empty() {
            return Ok(None);
        }
        let mut parts = payload.splitn(3, '\u{1f}');
        match parts.next() {
            Some("ok") => {
                let name = parts.next().unwrap_or_default().trim().to_string();
                let path = parts.next().unwrap_or_default().trim().to_string();
                if name.is_empty() || path.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(ImportedCa { name, path }))
                }
            }
            Some("err") => Err(anyhow!(
                parts
                    .next()
                    .unwrap_or("Android CA import failed")
                    .to_string()
            )),
            _ => Err(anyhow!("invalid Android CA import payload")),
        }
    }

    pub fn query_launch_config() -> Result<LaunchConfig> {
        with_main_activity(|env, activity| {
            Ok(LaunchConfig {
                server: launch_string_extra(env, activity, "io.hysteria.mobile.extra.SERVER")?,
                auth: launch_string_extra(env, activity, "io.hysteria.mobile.extra.AUTH")?,
                obfs_password: launch_string_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.OBFS_PASSWORD",
                )?,
                sni: launch_string_extra(env, activity, "io.hysteria.mobile.extra.SNI")?,
                ca_path: launch_string_extra(env, activity, "io.hysteria.mobile.extra.CA_PATH")?,
                pin_sha256: launch_string_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.PIN_SHA256",
                )?,
                bandwidth_up: launch_string_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.BANDWIDTH_UP",
                )?,
                bandwidth_down: launch_string_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.BANDWIDTH_DOWN",
                )?,
                quic_init_stream_receive_window: launch_string_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.QUIC_INIT_STREAM_RECEIVE_WINDOW",
                )?,
                quic_max_stream_receive_window: launch_string_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.QUIC_MAX_STREAM_RECEIVE_WINDOW",
                )?,
                quic_init_connection_receive_window: launch_string_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.QUIC_INIT_CONNECTION_RECEIVE_WINDOW",
                )?,
                quic_max_connection_receive_window: launch_string_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.QUIC_MAX_CONNECTION_RECEIVE_WINDOW",
                )?,
                quic_max_idle_timeout: launch_string_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.QUIC_MAX_IDLE_TIMEOUT",
                )?,
                quic_keep_alive_period: launch_string_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.QUIC_KEEP_ALIVE_PERIOD",
                )?,
                quic_disable_path_mtu_discovery: Some(launch_bool_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.QUIC_DISABLE_PATH_MTU_DISCOVERY",
                )?),
                insecure_tls: Some(launch_bool_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.INSECURE_TLS",
                )?),
                auto_connect: Some(launch_bool_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.AUTO_CONNECT",
                )?),
                auto_request_vpn: Some(launch_bool_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.AUTO_REQUEST_VPN",
                )?),
                auto_start_vpn: Some(launch_bool_extra(
                    env,
                    activity,
                    "io.hysteria.mobile.extra.AUTO_START_VPN",
                )?),
            })
        })
    }

    pub fn query_ca_catalog() -> Result<CaCatalog> {
        with_main_activity(|env, activity| {
            let directory = activity_string_method(env, activity, "caStorageDirFromRust")?;
            let listing = activity_string_method(env, activity, "caFilesListingFromRust")?;
            Ok(CaCatalog {
                directory,
                files: parse_ca_listing(&listing),
            })
        })
    }

    pub fn request_config_import() -> Result<()> {
        with_main_activity(|env, activity| {
            activity_void_method(env, activity, "launchConfigImportFromRust")
        })
    }

    pub fn take_config_import() -> Result<Option<ImportedConfig>> {
        with_main_activity(|env, activity| {
            let payload =
                activity_string_method(env, activity, "consumeImportedConfigResultFromRust")?;
            parse_imported_config_payload(&payload)
        })
    }

    pub fn request_ca_import() -> Result<()> {
        with_main_activity(|env, activity| {
            activity_void_method(env, activity, "launchCaImportFromRust")
        })
    }

    pub fn take_ca_import() -> Result<Option<ImportedCa>> {
        with_main_activity(|env, activity| {
            let payload = activity_string_method(env, activity, "consumeImportedCaResultFromRust")?;
            parse_imported_ca_payload(&payload)
        })
    }

    fn saved_profile_string(
        env: &mut Env<'_>,
        activity: &JObject<'static>,
        key: &str,
    ) -> Result<Option<String>> {
        let key = env
            .new_string(key)
            .context("failed to create saved profile key")?;
        let key_obj = JObject::from(key);
        let value = env
            .call_method(
                activity,
                jni_str!("savedProfileStringFromRust"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
                &[JValue::Object(&key_obj)],
            )
            .context("failed to query saved profile string")?
            .l()
            .context("invalid saved profile string")?;
        if value.is_null() {
            Ok(None)
        } else {
            let value = unsafe { JString::from_raw(env, value.into_raw().cast()) };
            Ok(Some(value.to_string()))
        }
    }

    fn saved_profile_bool(
        env: &mut Env<'_>,
        activity: &JObject<'static>,
        key: &str,
        default_value: bool,
    ) -> Result<bool> {
        let key = env
            .new_string(key)
            .context("failed to create saved profile bool key")?;
        let key_obj = JObject::from(key);
        env.call_method(
            activity,
            jni_str!("savedProfileBooleanFromRust"),
            jni_sig!("(Ljava/lang/String;Z)Z"),
            &[JValue::Object(&key_obj), JValue::Bool(default_value.into())],
        )
        .context("failed to query saved profile bool")?
        .z()
        .context("invalid saved profile bool")
    }

    pub fn query_saved_profile() -> Result<Option<FormState>> {
        with_main_activity(|env, activity| {
            let server = saved_profile_string(env, activity, "profile.server")?.unwrap_or_default();
            let auth = saved_profile_string(env, activity, "profile.auth")?.unwrap_or_default();
            let obfs_password =
                saved_profile_string(env, activity, "profile.obfs")?.unwrap_or_default();
            let sni = saved_profile_string(env, activity, "profile.sni")?.unwrap_or_default();
            let ca_path =
                saved_profile_string(env, activity, "profile.ca_path")?.unwrap_or_default();
            let pin_sha256 =
                saved_profile_string(env, activity, "profile.pin_sha256")?.unwrap_or_default();
            let bandwidth_up =
                saved_profile_string(env, activity, "profile.bandwidth.up")?.unwrap_or_default();
            let bandwidth_down =
                saved_profile_string(env, activity, "profile.bandwidth.down")?.unwrap_or_default();
            let quic_init_stream_receive_window =
                saved_profile_string(env, activity, "profile.quic.init_stream_receive_window")?
                    .unwrap_or_default();
            let quic_max_stream_receive_window =
                saved_profile_string(env, activity, "profile.quic.max_stream_receive_window")?
                    .unwrap_or_default();
            let quic_init_connection_receive_window =
                saved_profile_string(env, activity, "profile.quic.init_connection_receive_window")?
                    .unwrap_or_default();
            let quic_max_connection_receive_window =
                saved_profile_string(env, activity, "profile.quic.max_connection_receive_window")?
                    .unwrap_or_default();
            let quic_max_idle_timeout =
                saved_profile_string(env, activity, "profile.quic.max_idle_timeout")?
                    .unwrap_or_default();
            let quic_keep_alive_period =
                saved_profile_string(env, activity, "profile.quic.keep_alive_period")?
                    .unwrap_or_default();
            let quic_disable_path_mtu_discovery = saved_profile_bool(
                env,
                activity,
                "profile.quic.disable_path_mtu_discovery",
                false,
            )?;
            let insecure_tls = saved_profile_bool(env, activity, "profile.insecure_tls", true)?;

            if server.trim().is_empty()
                && auth.trim().is_empty()
                && obfs_password.trim().is_empty()
                && sni.trim().is_empty()
                && ca_path.trim().is_empty()
                && pin_sha256.trim().is_empty()
                && bandwidth_up.trim().is_empty()
                && bandwidth_down.trim().is_empty()
                && quic_init_stream_receive_window.trim().is_empty()
                && quic_max_stream_receive_window.trim().is_empty()
                && quic_init_connection_receive_window.trim().is_empty()
                && quic_max_connection_receive_window.trim().is_empty()
                && quic_max_idle_timeout.trim().is_empty()
                && quic_keep_alive_period.trim().is_empty()
                && !quic_disable_path_mtu_discovery
                && insecure_tls
            {
                return Ok(None);
            }

            Ok(Some(FormState {
                import_uri: String::new(),
                server,
                auth,
                obfs_password,
                sni,
                ca_path,
                pin_sha256,
                bandwidth_up,
                bandwidth_down,
                quic_init_stream_receive_window,
                quic_max_stream_receive_window,
                quic_init_connection_receive_window,
                quic_max_connection_receive_window,
                quic_max_idle_timeout,
                quic_keep_alive_period,
                quic_disable_path_mtu_discovery,
                insecure_tls,
            }))
        })
    }

    pub fn save_profile(form: &FormState) -> Result<()> {
        with_main_activity(|env, activity| {
            let server = env
                .new_string(form.server.as_str())
                .context("failed to create saved profile server string")?;
            let auth = env
                .new_string(form.auth.as_str())
                .context("failed to create saved profile auth string")?;
            let obfs_password = env
                .new_string(form.obfs_password.as_str())
                .context("failed to create saved profile obfs string")?;
            let sni = env
                .new_string(form.sni.as_str())
                .context("failed to create saved profile SNI string")?;
            let ca_path = env
                .new_string(form.ca_path.as_str())
                .context("failed to create saved profile CA path string")?;
            let pin_sha256 = env
                .new_string(form.pin_sha256.as_str())
                .context("failed to create saved profile pin string")?;
            let bandwidth_up = env
                .new_string(form.bandwidth_up.as_str())
                .context("failed to create saved profile bandwidth up string")?;
            let bandwidth_down = env
                .new_string(form.bandwidth_down.as_str())
                .context("failed to create saved profile bandwidth down string")?;
            let quic_init_stream_receive_window = env
                .new_string(form.quic_init_stream_receive_window.as_str())
                .context("failed to create saved profile QUIC init stream window string")?;
            let quic_max_stream_receive_window = env
                .new_string(form.quic_max_stream_receive_window.as_str())
                .context("failed to create saved profile QUIC max stream window string")?;
            let quic_init_connection_receive_window = env
                .new_string(form.quic_init_connection_receive_window.as_str())
                .context("failed to create saved profile QUIC init conn window string")?;
            let quic_max_connection_receive_window = env
                .new_string(form.quic_max_connection_receive_window.as_str())
                .context("failed to create saved profile QUIC max conn window string")?;
            let quic_max_idle_timeout = env
                .new_string(form.quic_max_idle_timeout.as_str())
                .context("failed to create saved profile QUIC idle timeout string")?;
            let quic_keep_alive_period = env
                .new_string(form.quic_keep_alive_period.as_str())
                .context("failed to create saved profile QUIC keepalive string")?;

            env.call_method(
                activity,
                jni_str!("saveProfileFromRust"),
                jni_sig!(
                    "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ZZ)V"
                ),
                &[
                    JValue::Object(&JObject::from(server)),
                    JValue::Object(&JObject::from(auth)),
                    JValue::Object(&JObject::from(obfs_password)),
                    JValue::Object(&JObject::from(sni)),
                    JValue::Object(&JObject::from(ca_path)),
                    JValue::Object(&JObject::from(pin_sha256)),
                    JValue::Object(&JObject::from(bandwidth_up)),
                    JValue::Object(&JObject::from(bandwidth_down)),
                    JValue::Object(&JObject::from(quic_init_stream_receive_window)),
                    JValue::Object(&JObject::from(quic_max_stream_receive_window)),
                    JValue::Object(&JObject::from(quic_init_connection_receive_window)),
                    JValue::Object(&JObject::from(quic_max_connection_receive_window)),
                    JValue::Object(&JObject::from(quic_max_idle_timeout)),
                    JValue::Object(&JObject::from(quic_keep_alive_period)),
                    JValue::Bool(form.quic_disable_path_mtu_discovery.into()),
                    JValue::Bool(form.insecure_tls.into()),
                ],
            )
            .context("failed to save Android profile")?;
            Ok(())
        })
    }

    pub fn clear_saved_profile() -> Result<()> {
        with_main_activity(|env, activity| {
            env.call_method(
                activity,
                jni_str!("clearSavedProfileFromRust"),
                jni_sig!("()V"),
                &[],
            )
            .context("failed to clear Android saved profile")?;
            Ok(())
        })
    }

    pub fn protect_fd(fd: i32) -> Result<bool> {
        with_vpn_service(|env, service| {
            let value = env
                .call_method(
                    service,
                    jni_str!("protectManagedFdFromRust"),
                    jni_sig!("(I)Z"),
                    &[JValue::Int(fd)],
                )
                .context("failed to call protectManagedFdFromRust")?;
            value
                .z()
                .context("invalid protectManagedFdFromRust return value")
        })
    }
}
#[cfg(not(target_os = "android"))]
mod imp {
    use anyhow::{Result, anyhow};

    use super::{CaCatalog, FormState, ImportedCa, ImportedConfig, LaunchConfig, VpnState};

    pub fn request_permission() -> Result<()> {
        Err(anyhow!(
            "Android VPN permission flow is only available on Android"
        ))
    }

    pub fn start_vpn_service(_socks_host: &str, _socks_port: i32) -> Result<()> {
        Err(anyhow!("Android VPN service is only available on Android"))
    }

    pub fn start_managed_vpn(_form: &FormState, _socks_host: &str, _socks_port: i32) -> Result<()> {
        Err(anyhow!("Android VPN service is only available on Android"))
    }

    pub fn stop_vpn_service() -> Result<()> {
        Err(anyhow!("Android VPN service is only available on Android"))
    }

    pub fn take_tun_fd() -> Result<i32> {
        Err(anyhow!("Android VPN service is only available on Android"))
    }

    pub fn query_state() -> Result<VpnState> {
        Ok(VpnState::default())
    }

    pub fn query_launch_config() -> Result<LaunchConfig> {
        Ok(LaunchConfig::default())
    }

    pub fn query_ca_catalog() -> Result<CaCatalog> {
        Ok(CaCatalog::default())
    }

    pub fn request_config_import() -> Result<()> {
        Err(anyhow!(
            "Android document import is only available on Android"
        ))
    }

    pub fn take_config_import() -> Result<Option<ImportedConfig>> {
        Ok(None)
    }

    pub fn request_ca_import() -> Result<()> {
        Err(anyhow!(
            "Android document import is only available on Android"
        ))
    }

    pub fn take_ca_import() -> Result<Option<ImportedCa>> {
        Ok(None)
    }

    pub fn query_saved_profile() -> Result<Option<FormState>> {
        Ok(None)
    }

    pub fn save_profile(_form: &FormState) -> Result<()> {
        Err(anyhow!(
            "Android profile persistence is only available on Android"
        ))
    }

    pub fn clear_saved_profile() -> Result<()> {
        Err(anyhow!(
            "Android profile persistence is only available on Android"
        ))
    }

    pub fn protect_fd(_fd: i32) -> Result<bool> {
        Ok(false)
    }
}

pub fn request_permission() -> Result<()> {
    imp::request_permission()
}

pub fn start_vpn_service(socks_host: &str, socks_port: i32) -> Result<()> {
    imp::start_vpn_service(socks_host, socks_port)
}

#[cfg(target_os = "android")]
pub fn cache_main_activity(
    env: &mut jni::Env<'_>,
    activity: &jni::objects::JObject<'_>,
) -> Result<()> {
    imp::cache_main_activity(env, activity)
}

#[cfg(target_os = "android")]
pub fn cache_vpn_service(
    env: &mut jni::Env<'_>,
    service: &jni::objects::JObject<'_>,
) -> Result<()> {
    imp::cache_vpn_service(env, service)
}

pub fn start_managed_vpn(form: &FormState, socks_host: &str, socks_port: u16) -> Result<()> {
    imp::start_managed_vpn(form, socks_host, i32::from(socks_port))
}

pub fn stop_vpn_service() -> Result<()> {
    imp::stop_vpn_service()
}

pub fn take_tun_fd() -> Result<i32> {
    imp::take_tun_fd()
}

pub fn query_state() -> Result<VpnState> {
    imp::query_state()
}

pub fn query_launch_config() -> Result<LaunchConfig> {
    imp::query_launch_config()
}

pub fn query_ca_catalog() -> Result<CaCatalog> {
    imp::query_ca_catalog()
}

pub fn request_config_import() -> Result<()> {
    imp::request_config_import()
}

pub fn take_config_import() -> Result<Option<ImportedConfig>> {
    imp::take_config_import()
}

pub fn request_ca_import() -> Result<()> {
    imp::request_ca_import()
}

pub fn take_ca_import() -> Result<Option<ImportedCa>> {
    imp::take_ca_import()
}

pub fn query_saved_profile() -> Result<Option<FormState>> {
    imp::query_saved_profile()
}

pub fn save_profile(form: &FormState) -> Result<()> {
    imp::save_profile(form)
}

pub fn clear_saved_profile() -> Result<()> {
    imp::clear_saved_profile()
}

#[cfg(target_os = "android")]
pub fn install_socket_protector() {
    let _ = hysteria_core::android::set_socket_protector(Arc::new(|fd| {
        let _ = imp::protect_fd(fd);
    }));
}

#[cfg(target_os = "android")]
pub fn cache_java_vm(env: &mut jni::Env<'_>) -> Result<()> {
    imp::cache_java_vm(env)
}

#[cfg(not(target_os = "android"))]
pub fn install_socket_protector() {}

#[cfg(not(target_os = "android"))]
pub fn cache_java_vm(_env: &mut ()) -> Result<()> {
    Ok(())
}

pub fn availability_message(state: &VpnState) -> Result<&'static str> {
    if state.available {
        Ok("Android VPN shell available")
    } else {
        Err(anyhow!(
            "Android VPN shell is only available on Android builds"
        ))
    }
}
