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
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};

    use dispatch2::{DispatchQueue, DispatchRetained};
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::ClassType;
    use objc2_core_foundation::{
        kCFMessagePortIsInvalid, kCFMessagePortSuccess, CFData, CFIndex, CFMessagePort,
        CFMessagePortContext, CFRetained, CFString,
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
        frame_clients: HashMap<String, RemotePort>,
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
                    eprintln!("karakuri-syphon: add info client `{client_id}`");
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
                            let res = send_surface_id(&remote_port, surface_id);
                            eprintln!("karakuri-syphon: sent initial surface_id {surface_id} to `{client_id}` (result={res})");
                        }
                    } else {
                        eprintln!("karakuri-syphon: failed to create remote port for info client `{client_id}`");
                    }
                }
            }
            SYPHON_MSG_ADD_CLIENT_FOR_FRAMES => {
                if let Some(ref client_id) = client_uuid {
                    eprintln!("karakuri-syphon: add frame client `{client_id}`");
                    let mut guard = match state_arc.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let remote_port = guard
                        .info_clients
                        .get(client_id)
                        .map(|p| p.0.clone())
                        .or_else(|| {
                            let remote_name = CFString::from_str(client_id);
                            CFMessagePort::new_remote(None, Some(&remote_name))
                        });
                    let surface_id = guard.surface_id;
                    if let Some(remote) = remote_port {
                        guard
                            .frame_clients
                            .insert(client_id.clone(), RemotePort(remote.clone()));
                        drop(guard);

                        if surface_id != 0 {
                            let res = send_new_frame(&remote);
                            eprintln!("karakuri-syphon: sent initial frame to `{client_id}` (result={res})");
                        }
                    } else {
                        eprintln!("karakuri-syphon: failed to create remote port for frame client `{client_id}`");
                    }
                }
            }
            SYPHON_MSG_REMOVE_CLIENT_FOR_INFO => {
                if let Some(ref client_id) = client_uuid {
                    eprintln!("karakuri-syphon: remove info client `{client_id}`");
                    let mut guard = match state_arc.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard.info_clients.remove(client_id);
                }
            }
            SYPHON_MSG_REMOVE_CLIENT_FOR_FRAMES => {
                if let Some(ref client_id) = client_uuid {
                    eprintln!("karakuri-syphon: remove frame client `{client_id}`");
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

    fn send_surface_id(port: &CFMessagePort, surface_id: u32) -> i32 {
        let num = NSNumber::new_u32(surface_id);
        let Ok(data) = (unsafe {
            NSKeyedArchiver::archivedDataWithRootObject_requiringSecureCoding_error(&num, true)
        }) else {
            return -1;
        };
        let bytes = unsafe { data.as_bytes_unchecked() };
        let Some(cf_data) = (unsafe { CFData::new(None, bytes.as_ptr(), bytes.len() as CFIndex) })
        else {
            return -1;
        };
        unsafe {
            port.send_request(
                SYPHON_MSG_UPDATE_SURFACE_ID,
                Some(&cf_data),
                1.0,
                0.0,
                None,
                std::ptr::null_mut(),
            )
        }
    }

    fn send_new_frame(port: &CFMessagePort) -> i32 {
        unsafe {
            port.send_request(
                SYPHON_MSG_NEW_FRAME,
                None,
                1.0,
                0.0,
                None,
                std::ptr::null_mut(),
            )
        }
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
                frame_clients: HashMap::new(),
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
                .map(|(k, v)| (k.clone(), v.0.clone()))
                .collect();

            drop(guard);

            let mut dead_info = Vec::new();
            if id_changed && surface_id != 0 {
                for (id, port) in &info_clients {
                    let res = send_surface_id(port, surface_id);
                    if res == kCFMessagePortIsInvalid {
                        dead_info.push(id.clone());
                    }
                }
            }

            let mut dead_frame = Vec::new();
            for (id, port) in &frame_clients {
                let res = send_new_frame(port);
                if res == kCFMessagePortIsInvalid {
                    dead_frame.push(id.clone());
                }
            }

            if !dead_info.is_empty() || !dead_frame.is_empty() {
                let mut guard = match self.state.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                for id in &dead_info {
                    eprintln!("karakuri-syphon: removing invalid info client `{id}`");
                    guard.info_clients.remove(id);
                }
                for id in &dead_frame {
                    eprintln!("karakuri-syphon: removing invalid frame client `{id}`");
                    guard.frame_clients.remove(id);
                }
            }
        }

        pub fn client_count(&self) -> u32 {
            let guard = match self.state.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.frame_clients.len().max(guard.info_clients.len()) as u32
        }

        pub fn stop(&mut self) {
            let (uuid, name, clients) = {
                let mut guard = match self.state.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let mut clients_map = HashMap::new();
                for (k, v) in &guard.info_clients {
                    clients_map.insert(k.clone(), v.0.clone());
                }
                for (k, v) in &guard.frame_clients {
                    clients_map.entry(k.clone()).or_insert_with(|| v.0.clone());
                }
                let clients: Vec<CFRetained<CFMessagePort>> = clients_map.into_values().collect();
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

    #[cfg(test)]
    pub(crate) fn test_port_sending() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static RECEIVED_MSG: AtomicU32 = AtomicU32::new(0);
        static RECEIVED_SURFACE: AtomicU32 = AtomicU32::new(0);

            unsafe extern "C-unwind" fn test_callback(
                _local: *mut CFMessagePort,
                msgid: i32,
                data: *const CFData,
                _info: *mut c_void,
            ) -> *const CFData {
                RECEIVED_MSG.store(msgid as u32, Ordering::SeqCst);
                if !data.is_null() {
                    let cf_data = unsafe { &*data };
                    let len = cf_data.length() as usize;
                    if len > 0 {
                        let slice = unsafe { std::slice::from_raw_parts(cf_data.byte_ptr(), len) };
                        let ns_data = objc2_foundation::NSData::with_bytes(slice);
                        if let Ok(unarchived) =
                            NSKeyedUnarchiver::unarchivedObjectOfClass_fromData_error(
                                NSNumber::class(),
                                &ns_data,
                            )
                        {
                            if let Ok(num) = unarchived.downcast::<NSNumber>() {
                                RECEIVED_SURFACE.store(num.as_u32(), Ordering::SeqCst);
                            }
                        }
                    }
                }
                std::ptr::null()
            }

            let port_name_str = "info.v002.Syphon.TestPort.12345";
            let port_name = CFString::from_str(port_name_str);
            let mut context = CFMessagePortContext {
                version: 0,
                info: std::ptr::null_mut(),
                retain: None,
                release: None,
                copyDescription: None,
            };
            let mut should_free = 0u8;

            let local_port = unsafe {
                CFMessagePort::new_local(
                    None,
                    Some(&port_name),
                    Some(test_callback),
                    &mut context,
                    &mut should_free,
                )
            }
            .expect("local port");

            let queue = DispatchQueue::new("test.queue", None);
            unsafe {
                local_port.set_dispatch_queue(Some(&queue));
            }

            let remote_port =
                CFMessagePort::new_remote(None, Some(&port_name)).expect("remote port");

            // Test sending surface ID
            let res_surface = send_surface_id(&remote_port, 4242);
            assert_eq!(res_surface, kCFMessagePortSuccess, "send_surface_id must succeed");

            // Wait a bit for dispatch queue
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert_eq!(
                RECEIVED_MSG.load(Ordering::SeqCst),
                SYPHON_MSG_UPDATE_SURFACE_ID as u32
            );
            assert_eq!(RECEIVED_SURFACE.load(Ordering::SeqCst), 4242);

            // Test sending new frame
            let res_frame = send_new_frame(&remote_port);
            assert_eq!(res_frame, kCFMessagePortSuccess, "send_new_frame must succeed");

            std::thread::sleep(std::time::Duration::from_millis(50));
            assert_eq!(
                RECEIVED_MSG.load(Ordering::SeqCst),
                SYPHON_MSG_NEW_FRAME as u32
            );

            local_port.invalidate();
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

    #[test]
    #[cfg(target_os = "macos")]
    fn test_string_and_number_serialization() {
        use objc2::ClassType;
        use objc2_foundation::{NSKeyedArchiver, NSKeyedUnarchiver, NSNumber, NSString};

        let orig_str = NSString::from_str("my-client-uuid-12345");
        let data = unsafe {
            NSKeyedArchiver::archivedDataWithRootObject_requiringSecureCoding_error(&orig_str, true)
                .expect("archive string")
        };
        let unarchived = unsafe {
            NSKeyedUnarchiver::unarchivedObjectOfClass_fromData_error(NSString::class(), &data)
                .expect("unarchive string")
        };
        let decoded_str = unarchived.downcast::<NSString>().expect("downcast NSString");
        assert_eq!(decoded_str.to_string(), "my-client-uuid-12345");

        let orig_num = NSNumber::new_u32(9999);
        let num_data = unsafe {
            NSKeyedArchiver::archivedDataWithRootObject_requiringSecureCoding_error(&orig_num, true)
                .expect("archive number")
        };
        let unarchived_num = unsafe {
            NSKeyedUnarchiver::unarchivedObjectOfClass_fromData_error(NSNumber::class(), &num_data)
                .expect("unarchive number")
        };
        let decoded_num = unarchived_num.downcast::<NSNumber>().expect("downcast NSNumber");
        assert_eq!(decoded_num.as_u32(), 9999);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_message_port_sending() {
        macos::test_port_sending();
    }
}
