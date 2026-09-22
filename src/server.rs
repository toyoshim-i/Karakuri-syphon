//! Syphon protocol server implementation for macOS.
//!
//! Syphon is an open-source macOS technology for sharing video frames between
//! applications using IOSurface. The communication protocol runs over CoreFoundation
//! Mach message ports (`CFMessagePort`) and `NSDistributedNotificationCenter`.
//!
//! This module implements the Syphon protocol natively in pure Rust using `objc2`
//! and `objc2-core-foundation`, requiring no foreign compilers or external C libraries.

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::{HashMap, HashSet};
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};

    use dispatch2::{DispatchQueue, DispatchRetained};
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::ClassType;
    use objc2_core_foundation::{
        kCFMessagePortSuccess, CFData, CFIndex, CFMessagePort, CFMessagePortContext, CFRetained,
        CFString,
    };
    use objc2_foundation::{
        ns_string, NSArray, NSDictionary, NSDistributedNotificationCenter, NSKeyedArchiver,
        NSKeyedUnarchiver, NSNumber, NSString, NSUUID,
    };

    // Syphon message types for messages received by the server:
    const SYPHON_MSG_ADD_CLIENT_FOR_INFO: i32 = 0;
    const SYPHON_MSG_ADD_CLIENT_FOR_FRAMES: i32 = 1;
    const SYPHON_MSG_REMOVE_CLIENT_FOR_INFO: i32 = 2;
    const SYPHON_MSG_REMOVE_CLIENT_FOR_FRAMES: i32 = 3;

    // Syphon message types for messages sent by the server to clients:
    const SYPHON_MSG_NEW_FRAME: i32 = 1;
    const SYPHON_MSG_UPDATE_SURFACE_ID: i32 = 2;
    const SYPHON_MSG_RETIRE_SERVER: i32 = 3;

    struct RemotePort(CFRetained<CFMessagePort>);
    unsafe impl Send for RemotePort {}
    unsafe impl Sync for RemotePort {}

    struct ServerState {
        uuid: String,
        name: String,
        surface_id: u32,
        info_clients: HashMap<String, RemotePort>,
        frame_clients: HashSet<String>,
    }

    unsafe extern "C-unwind" fn retain_state(info: *const c_void) -> *const c_void {
        Arc::increment_strong_count(info as *const Mutex<ServerState>);
        info
    }

    unsafe extern "C-unwind" fn release_state(info: *const c_void) {
        Arc::decrement_strong_count(info as *const Mutex<ServerState>);
    }

    unsafe extern "C-unwind" fn message_callback(
        _local: *mut CFMessagePort,
        msgid: i32,
        data: *const CFData,
        info: *mut c_void,
    ) -> *const CFData {
        if info.is_null() {
            return std::ptr::null();
        }
        let state_arc = &*(info as *const Mutex<ServerState>);

        // Decode client UUID from payload if present.
        let client_uuid = if !data.is_null() {
            let cf_data = &*data;
            let len = cf_data.length() as usize;
            if len > 0 {
                let slice = std::slice::from_raw_parts(cf_data.byte_ptr(), len);
                let ns_data = objc2_foundation::NSData::with_bytes(slice);
                NSKeyedUnarchiver::unarchivedObjectOfClass_fromData_error(
                    NSString::class(),
                    &ns_data,
                )
                .ok()
                .and_then(|obj| obj.downcast::<NSString>().ok())
                .map(|s| s.to_string())
            } else {
                None
            }
        } else {
            None
        };

        match msgid {
            SYPHON_MSG_ADD_CLIENT_FOR_INFO => {
                if let Some(ref client_id) = client_uuid {
                    let remote_name = CFString::from_str(client_id);
                    if let Some(remote_port) = CFMessagePort::new_remote(None, Some(&remote_name)) {
                        let mut guard = match state_arc.lock() {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        let surface_id = guard.surface_id;
                        guard
                            .info_clients
                            .insert(client_id.clone(), RemotePort(remote_port.clone()));
                        drop(guard);

                        if surface_id != 0 {
                            let _ = send_surface_id(&remote_port, surface_id);
                        }
                    }
                }
            }
            SYPHON_MSG_ADD_CLIENT_FOR_FRAMES => {
                if let Some(ref client_id) = client_uuid {
                    let mut guard = match state_arc.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard.frame_clients.insert(client_id.clone());
                    let surface_id = guard.surface_id;
                    let remote = guard.info_clients.get(client_id).map(|p| p.0.clone());
                    drop(guard);

                    if surface_id != 0 {
                        if let Some(remote) = remote {
                            let _ = send_new_frame(&remote);
                        }
                    }
                }
            }
            SYPHON_MSG_REMOVE_CLIENT_FOR_INFO => {
                if let Some(ref client_id) = client_uuid {
                    let mut guard = match state_arc.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard.info_clients.remove(client_id);
                }
            }
            SYPHON_MSG_REMOVE_CLIENT_FOR_FRAMES => {
                if let Some(ref client_id) = client_uuid {
                    let mut guard = match state_arc.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard.frame_clients.remove(client_id);
                }
            }
            _ => {}
        }

        std::ptr::null()
    }

    fn send_surface_id(port: &CFMessagePort, surface_id: u32) -> bool {
        let num = NSNumber::new_u32(surface_id);
        let Ok(data) = (unsafe {
            NSKeyedArchiver::archivedDataWithRootObject_requiringSecureCoding_error(&num, true)
        }) else {
            return false;
        };
        let bytes = unsafe { data.as_bytes_unchecked() };
        let Some(cf_data) = (unsafe { CFData::new(None, bytes.as_ptr(), bytes.len() as CFIndex) })
        else {
            return false;
        };
        let res = unsafe {
            port.send_request(
                SYPHON_MSG_UPDATE_SURFACE_ID,
                Some(&cf_data),
                1.0,
                0.0,
                None,
                std::ptr::null_mut(),
            )
        };
        res == kCFMessagePortSuccess
    }

    fn send_new_frame(port: &CFMessagePort) -> bool {
        let res = unsafe {
            port.send_request(
                SYPHON_MSG_NEW_FRAME,
                None,
                1.0,
                0.0,
                None,
                std::ptr::null_mut(),
            )
        };
        res == kCFMessagePortSuccess
    }

    fn send_retire(port: &CFMessagePort) -> bool {
        let res = unsafe {
            port.send_request(
                SYPHON_MSG_RETIRE_SERVER,
                None,
                1.0,
                0.0,
                None,
                std::ptr::null_mut(),
            )
        };
        res == kCFMessagePortSuccess
    }

    fn broadcast(notification_name: &NSString, uuid: &str, server_name: &str) {
        let center = NSDistributedNotificationCenter::defaultCenter();
        let uuid_ns = NSString::from_str(uuid);
        let name_ns = NSString::from_str(server_name);

        let surface_dict = NSDictionary::from_slices(
            &[ns_string!("SyphonSurfaceType")],
            &[ns_string!("SyphonSurfaceTypeIOSurface")],
        );
        let surfaces = NSArray::from_retained_slice(&[surface_dict]);

        let k1 = ns_string!("SyphonServerDescriptionDictionaryVersionKey");
        let k2 = ns_string!("SyphonServerDescriptionNameKey");
        let k3 = ns_string!("SyphonServerDescriptionUUIDKey");
        let k4 = ns_string!("SyphonServerDescriptionAppNameKey");
        let k5 = ns_string!("SyphonServerDescriptionSurfacesKey");

        let v1 = NSNumber::new_u32(0);
        let v4 = ns_string!("Karakuri");

        let keys = [k1, k2, k3, k4, k5];
        let values: [&AnyObject; 5] = [
            v1.as_super(),
            name_ns.as_super(),
            uuid_ns.as_super(),
            v4.as_super(),
            surfaces.as_super(),
        ];

        let user_info: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&keys, &values);
        let user_info_ref: &NSDictionary = unsafe {
            &*(&*user_info as *const NSDictionary<NSString, AnyObject> as *const NSDictionary)
        };

        unsafe {
            center.postNotificationName_object_userInfo_deliverImmediately(
                notification_name,
                Some(&uuid_ns),
                Some(user_info_ref),
                true,
            );
        }
    }

    /// Native macOS Syphon server.
    pub struct SyphonServer {
        state: Arc<Mutex<ServerState>>,
        local_port: CFRetained<CFMessagePort>,
        _queue: DispatchRetained<DispatchQueue>,
    }

    impl SyphonServer {
        pub fn new(server_name: &str) -> Result<Self, String> {
            let raw_uuid = NSUUID::UUID().UUIDString().to_string();
            let uuid = format!("info.v002.Syphon.{}", raw_uuid);

            let state = Arc::new(Mutex::new(ServerState {
                uuid: uuid.clone(),
                name: server_name.to_string(),
                surface_id: 0,
                info_clients: HashMap::new(),
                frame_clients: HashSet::new(),
            }));

            let port_name = CFString::from_str(&uuid);
            let mut context = CFMessagePortContext {
                version: 0,
                info: Arc::into_raw(state.clone()) as *mut c_void,
                retain: Some(retain_state),
                release: Some(release_state),
                copyDescription: None,
            };
            let mut should_free = 0u8;

            let local_port = unsafe {
                CFMessagePort::new_local(
                    None,
                    Some(&port_name),
                    Some(message_callback),
                    &mut context,
                    &mut should_free,
                )
            }
            .ok_or_else(|| format!("failed to create CFMessagePort with name `{uuid}`"))?;

            let queue_label = format!("{}.dispatch", uuid);
            let queue = DispatchQueue::new(&queue_label, None);
            unsafe {
                local_port.set_dispatch_queue(Some(&queue));
            }

            // Announce server to the system.
            broadcast(
                ns_string!("info.v002.Syphon.ServerAnnounce"),
                &uuid,
                server_name,
            );

            eprintln!("karakuri-syphon: Syphon server published as `{server_name}` (UUID: {uuid})");

            Ok(Self {
                state,
                local_port,
                _queue: queue,
            })
        }

        pub fn publish_surface(&mut self, surface_id: u32) {
            let mut guard = match self.state.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };

            let id_changed = guard.surface_id != surface_id;
            if id_changed {
                guard.surface_id = surface_id;
            }

            let info_clients: Vec<(String, CFRetained<CFMessagePort>)> = guard
                .info_clients
                .iter()
                .map(|(k, v)| (k.clone(), v.0.clone()))
                .collect();

            let frame_clients: Vec<(String, CFRetained<CFMessagePort>)> = guard
                .frame_clients
                .iter()
                .filter_map(|k| guard.info_clients.get(k).map(|v| (k.clone(), v.0.clone())))
                .collect();

            drop(guard);

            let mut dead_info = Vec::new();
            if id_changed && surface_id != 0 {
                for (id, port) in &info_clients {
                    if !send_surface_id(port, surface_id) {
                        dead_info.push(id.clone());
                    }
                }
            }

            let mut dead_frame = Vec::new();
            for (id, port) in &frame_clients {
                if !send_new_frame(port) {
                    dead_frame.push(id.clone());
                }
            }

            if !dead_info.is_empty() || !dead_frame.is_empty() {
                let mut guard = match self.state.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                for id in dead_info {
                    guard.info_clients.remove(&id);
                }
                for id in dead_frame {
                    guard.frame_clients.remove(&id);
                }
            }
        }

        pub fn client_count(&self) -> u32 {
            let guard = match self.state.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.info_clients.len() as u32
        }

        pub fn stop(&mut self) {
            let (uuid, name, clients) = {
                let mut guard = match self.state.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let clients: Vec<CFRetained<CFMessagePort>> =
                    guard.info_clients.values().map(|p| p.0.clone()).collect();
                guard.info_clients.clear();
                guard.frame_clients.clear();
                (guard.uuid.clone(), guard.name.clone(), clients)
            };

            for port in &clients {
                let _ = send_retire(port);
            }

            self.local_port.invalidate();

            broadcast(ns_string!("info.v002.Syphon.ServerRetire"), &uuid, &name);
            eprintln!("karakuri-syphon: Syphon server retired (`{name}`)");
        }
    }

    impl Drop for SyphonServer {
        fn drop(&mut self) {
            self.stop();
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::SyphonServer;

#[cfg(not(target_os = "macos"))]
pub struct SyphonServer;

#[cfg(not(target_os = "macos"))]
impl SyphonServer {
    pub fn new(_server_name: &str) -> Result<Self, String> {
        Err("Syphon is only supported on macOS".to_string())
    }

    pub fn publish_surface(&mut self, _surface_id: u32) {}

    pub fn client_count(&self) -> u32 {
        0
    }

    pub fn stop(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn test_syphon_server_lifecycle() {
        let mut server = SyphonServer::new("TestServer").expect("create server");
        assert_eq!(server.client_count(), 0);
        server.publish_surface(42);
        server.publish_surface(42);
        server.publish_surface(43);
        assert_eq!(server.client_count(), 0);
        server.stop();
    }
}
