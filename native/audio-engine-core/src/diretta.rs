#[cfg(any(feature = "diretta", test))]
use std::path::Path;

#[cfg(any(feature = "diretta", test))]
use anyhow::anyhow;
use anyhow::Result;

#[cfg(any(feature = "diretta", test))]
use crate::direct_dsd::DirectDsdFormat;
#[cfg(any(feature = "diretta", test))]
use crate::direct_pcm::DirectPcmFormat;

pub const DEVICE_PREFIX: &str = "diretta:";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirettaDevice {
    pub id: String,
    pub name: String,
    pub ipv6_addr: String,
    pub full_addr: String,
    pub if_idx: i32,
    pub target_name: String,
    pub output_name: String,
    pub model_name: String,
    pub mtu: u32,
}

pub fn selector_target(selector: &str) -> Option<&str> {
    selector
        .strip_prefix(DEVICE_PREFIX)
        .filter(|value| !value.is_empty() && *value != "undefined" && *value != "null")
}

pub fn selector_for(target_id: &str) -> String {
    format!("{DEVICE_PREFIX}{target_id}")
}

#[cfg(feature = "diretta")]
mod imp {
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::ptr::NonNull;

    use super::*;
    use crate::direct_dsd::{
        direct_dsd_next_block, direct_dsd_release_block, DirectDsdBitOrder, DirectDsdMonitor,
        DirectDsdSource, DirectDsdStageHandle,
    };
    use crate::direct_pcm::{
        direct_pcm_next_block, direct_pcm_release_block, DirectPcmMonitor, DirectPcmSource,
        DirectPcmStageHandle,
    };
    use diretta_sys::{
        splayer_diretta_close, splayer_diretta_last_error, splayer_diretta_open_direct,
        splayer_diretta_open_dsd_direct, splayer_diretta_pause, splayer_diretta_play,
        splayer_diretta_scan, SPlayerDirettaDevice, TEXT_CAPACITY,
    };

    const MAX_SCAN_DEVICES: usize = 32;

    fn last_error(fallback: &str) -> anyhow::Error {
        let message = unsafe {
            let ptr = splayer_diretta_last_error();
            if ptr.is_null() {
                None
            } else {
                CStr::from_ptr(ptr)
                    .to_str()
                    .ok()
                    .filter(|value| !value.is_empty())
            }
        };
        anyhow!(message.unwrap_or(fallback).to_owned())
    }

    fn raw_text(value: &[c_char; TEXT_CAPACITY]) -> String {
        unsafe { CStr::from_ptr(value.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn available() -> bool {
        true
    }

    pub fn scan_devices() -> Result<Vec<DirettaDevice>> {
        let mut raw = vec![SPlayerDirettaDevice::default(); MAX_SCAN_DEVICES];
        let discovered = unsafe { splayer_diretta_scan(raw.as_mut_ptr(), raw.len()) };
        if discovered == 0 {
            let error = last_error("");
            if error.to_string().is_empty() {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        let count = discovered.min(raw.len());
        Ok(raw
            .into_iter()
            .take(count)
            .filter_map(|device| {
                let id = raw_text(&device.id);
                if id.is_empty() {
                    return None;
                }
                let name = raw_text(&device.name);
                let ipv6_addr = raw_text(&device.ipv6_addr);
                let full_addr = raw_text(&device.full_addr);
                let target_name = raw_text(&device.target_name);
                let output_name = raw_text(&device.output_name);
                let model_name = raw_text(&device.model_name);
                Some(DirettaDevice {
                    id: selector_for(&id),
                    name: if name.is_empty() {
                        "Diretta Target".to_string()
                    } else {
                        name
                    },
                    ipv6_addr: if ipv6_addr.is_empty() { id.clone() } else { ipv6_addr },
                    full_addr: if full_addr.is_empty() { id } else { full_addr },
                    if_idx: device.if_idx,
                    target_name,
                    output_name,
                    model_name,
                    mtu: if device.mtu > 0 { device.mtu } else { 1500 },
                })
            })
            .collect())
    }

    pub struct DirettaDirectConnection {
        raw: NonNull<c_void>,
        source: DirectPcmSource,
    }

    // Diretta SDK handle 只由拥有者线程控制；SDK 内部发送线程仅通过回调访问 source ring。
    unsafe impl Send for DirettaDirectConnection {}

    impl DirettaDirectConnection {
        pub fn open_local(selector: &str, path: &Path) -> Result<Self> {
            let (connection, _) = Self::open_local_at(selector, path, 0.0)?;
            Ok(connection)
        }

        pub fn open_local_at(
            selector: &str,
            path: &Path,
            position_secs: f64,
        ) -> Result<(Self, f64)> {
            let target = selector_target(selector)
                .ok_or_else(|| anyhow!("invalid Diretta output selector"))?;
            let target = CString::new(target).map_err(|_| anyhow!("invalid Diretta target id"))?;
            let (source, actual_position) = DirectPcmSource::open_local_at(path, position_secs)?;
            let format = source.format();
            let raw = unsafe {
                splayer_diretta_open_direct(
                    target.as_ptr(),
                    format.sample_rate,
                    format.channels,
                    format.storage_bits,
                    source.callback_context(),
                    direct_pcm_next_block,
                    direct_pcm_release_block,
                )
            };
            let raw = NonNull::new(raw)
                .ok_or_else(|| last_error("failed to open Diretta Source Direct target"))?;
            Ok((Self { raw, source }, actual_position))
        }

        pub fn format(&self) -> DirectPcmFormat {
            self.source.format()
        }

        pub fn play(&mut self) -> Result<()> {
            if unsafe { splayer_diretta_play(self.raw.as_ptr()) } {
                Ok(())
            } else {
                Err(last_error("failed to start Diretta Source Direct playback"))
            }
        }

        pub fn pause(&mut self) -> Result<()> {
            if unsafe { splayer_diretta_pause(self.raw.as_ptr()) } {
                Ok(())
            } else {
                Err(last_error("failed to pause Diretta Source Direct playback"))
            }
        }

        pub fn seek_while_paused(&mut self, position_secs: f64) -> Result<f64> {
            self.source.seek_while_paused(position_secs)
        }

        pub fn replace_local_source_while_paused(
            &mut self,
            path: &Path,
        ) -> Result<DirectPcmFormat> {
            self.source.replace_local_while_paused(path)
        }

        pub fn failed(&self) -> bool {
            self.source.failed()
        }

        pub fn finished(&self) -> bool {
            self.source.finished()
        }

        pub fn consumed_position(&self) -> f64 {
            self.source.consumed_position()
        }

        pub fn monitor(&self) -> DirectPcmMonitor {
            self.source.monitor()
        }

        pub fn stage_handle(&self) -> DirectPcmStageHandle {
            self.source.stage_handle()
        }

        pub fn set_duration(&self, duration_secs: f64) {
            self.source.set_duration(duration_secs);
        }
    }

    impl Drop for DirettaDirectConnection {
        fn drop(&mut self) {
            // 必须先关闭 SDK，确保发送线程不再持有 source callback context，再释放 source ring。
            unsafe { splayer_diretta_close(self.raw.as_ptr()) };
        }
    }

    pub struct DirettaDirectDsdConnection {
        raw: NonNull<c_void>,
        source: DirectDsdSource,
    }

    unsafe impl Send for DirettaDirectDsdConnection {}

    impl DirettaDirectDsdConnection {
        pub fn open_local(selector: &str, path: &Path) -> Result<Self> {
            let (connection, _) = Self::open_local_at(selector, path, 0.0)?;
            Ok(connection)
        }

        pub fn open_local_at(
            selector: &str,
            path: &Path,
            position_secs: f64,
        ) -> Result<(Self, f64)> {
            let target = selector_target(selector)
                .ok_or_else(|| anyhow!("invalid Diretta output selector"))?;
            let target = CString::new(target).map_err(|_| anyhow!("invalid Diretta target id"))?;
            let (mut source, actual_position) =
                DirectDsdSource::open_local_at(path, position_secs)?;
            let format = source.format();
            let source_lsb_first = format.bit_order == DirectDsdBitOrder::LsbFirst;
            let mut wire_lsb_first = source_lsb_first;
            let raw = unsafe {
                splayer_diretta_open_dsd_direct(
                    target.as_ptr(),
                    format.bit_rate,
                    format.channels,
                    source_lsb_first,
                    &mut wire_lsb_first,
                    source.callback_context(),
                    direct_dsd_next_block,
                    direct_dsd_release_block,
                )
            };
            let raw = NonNull::new(raw)
                .ok_or_else(|| last_error("failed to open Diretta Native DSD target"))?;
            let wire_bit_order = if wire_lsb_first {
                DirectDsdBitOrder::LsbFirst
            } else {
                DirectDsdBitOrder::MsbFirst
            };
            if wire_bit_order != format.bit_order {
                if let Err(error) =
                    source.set_wire_bit_order_while_paused(wire_bit_order, actual_position)
                {
                    unsafe { splayer_diretta_close(raw.as_ptr()) };
                    return Err(anyhow!(
                        "failed to adapt Native DSD wire bit order: {error}"
                    ));
                }
            }
            Ok((Self { raw, source }, actual_position))
        }

        pub fn format(&self) -> DirectDsdFormat {
            self.source.format()
        }

        pub fn play(&mut self) -> Result<()> {
            if unsafe { splayer_diretta_play(self.raw.as_ptr()) } {
                Ok(())
            } else {
                Err(last_error("failed to start Diretta Native DSD playback"))
            }
        }

        pub fn pause(&mut self) -> Result<()> {
            if unsafe { splayer_diretta_pause(self.raw.as_ptr()) } {
                Ok(())
            } else {
                Err(last_error("failed to pause Diretta Native DSD playback"))
            }
        }

        pub fn seek_while_paused(&mut self, position_secs: f64) -> Result<f64> {
            self.source.seek_while_paused(position_secs)
        }

        pub fn replace_local_source_while_paused(
            &mut self,
            path: &Path,
        ) -> Result<DirectDsdFormat> {
            self.source.replace_local_while_paused(path)
        }

        pub fn failed(&self) -> bool {
            self.source.failed()
        }

        pub fn finished(&self) -> bool {
            self.source.finished()
        }

        pub fn consumed_position(&self) -> f64 {
            self.source.consumed_position()
        }

        pub fn monitor(&self) -> DirectDsdMonitor {
            self.source.monitor()
        }

        pub fn stage_handle(&self) -> DirectDsdStageHandle {
            self.source.stage_handle()
        }
    }

    impl Drop for DirettaDirectDsdConnection {
        fn drop(&mut self) {
            unsafe { splayer_diretta_close(self.raw.as_ptr()) };
        }
    }
}

#[cfg(not(feature = "diretta"))]
mod imp {
    use super::*;

    pub fn available() -> bool {
        false
    }

    pub fn scan_devices() -> Result<Vec<DirettaDevice>> {
        Ok(Vec::new())
    }

    #[cfg(test)]
    pub struct DirettaDirectConnection;
    #[cfg(test)]
    pub struct DirettaDirectDsdConnection;

    #[cfg(test)]
    impl DirettaDirectConnection {
        pub fn open_local(_selector: &str, _path: &Path) -> Result<Self> {
            Err(anyhow!(
                "Diretta SDK support is not compiled; set DIRETTA_SDK_DIR when building"
            ))
        }

        pub fn format(&self) -> DirectPcmFormat {
            unreachable!("Diretta SDK support is not compiled")
        }

        pub fn play(&mut self) -> Result<()> {
            Err(anyhow!("Diretta SDK support is not compiled"))
        }

        pub fn pause(&mut self) -> Result<()> {
            Err(anyhow!("Diretta SDK support is not compiled"))
        }

        pub fn failed(&self) -> bool {
            true
        }

        pub fn finished(&self) -> bool {
            false
        }
    }

    #[cfg(test)]
    impl DirettaDirectDsdConnection {
        pub fn open_local(_selector: &str, _path: &Path) -> Result<Self> {
            Err(anyhow!("Diretta SDK support is not compiled"))
        }

        pub fn format(&self) -> DirectDsdFormat {
            unreachable!("Diretta SDK support is not compiled")
        }

        pub fn play(&mut self) -> Result<()> {
            Err(anyhow!("Diretta SDK support is not compiled"))
        }

        pub fn pause(&mut self) -> Result<()> {
            Err(anyhow!("Diretta SDK support is not compiled"))
        }

        pub fn failed(&self) -> bool {
            true
        }

        pub fn finished(&self) -> bool {
            false
        }
    }
}

pub use imp::scan_devices;
#[cfg(feature = "diretta")]
pub use imp::{DirettaDirectConnection, DirettaDirectDsdConnection};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_pull_callback_stays_copy_free_and_lock_free() {
        let bridge = include_str!("../../diretta-sys/src/bridge.cpp");
        let callback = bridge
            .split("bool getNewStream(diretta_stream& stream) override")
            .nth(1)
            .and_then(|tail| tail.split("private:").next())
            .expect("Diretta pull callback body should exist");
        for forbidden in [
            "memcpy",
            "setStream",
            "resize(",
            "mutex",
            "new ",
            "findOutput",
            "setSink(",
        ] {
            assert!(
                !callback.contains(forbidden),
                "Diretta SDK real-time callback must not contain {forbidden}"
            );
        }
        assert!(callback.contains("stream.Data.P"));
        assert!(callback.contains("stream.Size"));
    }

    #[test]
    fn native_dsd_bridge_selects_a_supported_native_wire_bit_order_only() {
        let bridge = include_str!("../../diretta-sys/src/bridge.cpp");
        let dsd_open = bridge
            .split("void* splayer_diretta_open_dsd_direct(")
            .nth(1)
            .and_then(|tail| tail.split("bool splayer_diretta_play").next())
            .expect("Native DSD bridge entry should exist");
        assert!(dsd_open.contains("FMT_DSD1"));
        assert!(dsd_open.contains("FMT_DSD_SIZ_32"));
        assert!(dsd_open.contains("FMT_DSD_BIG"));
        assert!(dsd_open.contains("FMT_DSD_LSB"));
        assert!(dsd_open.contains("FMT_DSD_MSB"));
        assert!(dsd_open.contains("source_lsb_first"));
        assert!(dsd_open.contains("wire_lsb_first"));
        assert!(dsd_open.contains("alternate_format_id"));
        for forbidden in ["DSD2PCM", "DoP", "memcpy", "reverse_bits"] {
            assert!(
                !dsd_open.contains(forbidden),
                "Native DSD bridge must not perform sample-domain conversion {forbidden}"
            );
        }
    }

    #[test]
    fn bridge_keeps_connection_completion_separate_from_playback_online_state() {
        let bridge = include_str!("../../diretta-sys/src/bridge.cpp");
        let connection = bridge
            .split("struct DirettaConnection")
            .nth(1)
            .and_then(|tail| tail.split("bool discover").next())
            .expect("Diretta connection cleanup should exist");
        assert!(connection.contains("~DirettaConnection()"));
        assert!(connection.contains("shutdown();"));
        assert!(connection.contains("disconnect_flgset();"));
        assert!(connection.contains("disconnect(true);"));
        assert!(connection.contains("disconnectWait();"));
        assert!(connection.contains("sync->close();"));
        assert!(!connection.contains("disconnect(false);"));
        assert!(!connection.contains("kDisconnectTimeout"));

        let open = bridge
            .split("void* open_direct_with_format(")
            .nth(1)
            .and_then(|tail| tail.split("} // namespace").next())
            .expect("Diretta open path should exist");
        assert!(open.contains("connectPrepare()"));
        assert!(open.contains("connect(0)"));
        assert!(open.contains("connectWait()"));
        assert!(!open.contains("is_online()"));
        assert!(!open.contains("->play()"));

        let close = bridge
            .split("void splayer_diretta_close(void* opaque)")
            .nth(1)
            .expect("Diretta close entry should exist");
        assert!(close.contains("unique_ptr<DirettaConnection>"));
    }

    #[cfg(not(feature = "diretta"))]
    #[test]
    fn builds_without_sdk_and_reports_no_discoverable_targets() {
        assert!(!imp::available());
        assert!(imp::scan_devices().unwrap().is_empty());
    }

    #[test]
    fn selectors_keep_diretta_targets_in_the_existing_device_id_space() {
        let selector = selector_for("fe80::1234%2");
        assert_eq!(selector, "diretta:fe80::1234%2");
        assert_eq!(selector_target(&selector), Some("fe80::1234%2"));
        assert_eq!(selector_target("default_output"), None);
        assert_eq!(selector_target("diretta:"), None);
    }
}
