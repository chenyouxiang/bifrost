use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use parking_lot::{RwLock, Mutex};
use threadpool::ThreadPool;
use num_cpus;
use bifrost_hasher::{hash_str, hash_bytes};
use raft::{RaftService, IS_LEADER};
use rpc;
use utils::bincode;
use serde;
use super::super::{OpType};
use super::super::super::RaftMsg;
use super::*;


lazy_static! {
    pub static ref THREAD_POOL: Mutex<ThreadPool> = Mutex::new(ThreadPool::new(num_cpus::get()));
}

pub struct Subscriber {
    pub session_id: u64,
    pub client: Arc<SyncServiceClient>
}

pub struct Subscriptions {
    next_id: u64,
    subscribers: HashMap<u64, Subscriber>,
    suber_subs: HashMap<u64, HashSet<u64>>, //suber_id -> sub_id
    subscriptions: HashMap<SubKey, HashSet<u64>>, // key -> sub_id
    sub_suber: HashMap<u64, u64>,
    sub_to_key: HashMap<u64, SubKey>, //sub_id -> sub_key
}

impl Subscriptions {

    pub fn new() -> Subscriptions {
        Subscriptions {
            next_id: 0,
            subscribers: HashMap::new(),
            suber_subs: HashMap::new(),
            subscriptions: HashMap::new(),
            sub_suber: HashMap::new(),
            sub_to_key: HashMap::new(),
        }
    }

    pub fn subscribe(&mut self, key: SubKey, address: &String, session_id: u64) -> Result<u64, ()> {
        let sub_service_id = DEFAULT_SERVICE_ID;
        let suber_id = hash_str(address);
        let suber_exists = self.subscribers.contains_key(&suber_id);
        let sub_id = self.next_id;
        let require_reload_suber = if suber_exists {
            let session_match = self.subscribers.get(&suber_id).unwrap().session_id == session_id;
            if !session_match {
                self.remove_subscriber(suber_id);
                true
            } else {false}
        } else {true};
        if !self.subscribers.contains_key(&suber_id) {
            self.subscribers.insert(suber_id, Subscriber {
                session_id: session_id,
                client: {
                    if let Ok(client) = rpc::DEFAULT_CLIENT_POOL.get(address) {
                        SyncServiceClient::new(sub_service_id, &client)
                    } else {
                        return Err(());
                    }
                }
            });
        }
        self.suber_subs.entry(suber_id).or_insert_with(|| HashSet::new()).insert(sub_id);
        self.subscriptions.entry(key).or_insert_with(|| HashSet::new()).insert(sub_id);
        self.sub_to_key.insert(sub_id, key);
        self.sub_suber.insert(sub_id, suber_id);

        self.next_id += 1;
        Ok(sub_id)
    }

    pub fn remove_subscriber(&mut self, suber_id: u64) {
        let suber_subs = if let Some(sub_ids) = self.suber_subs.get(&suber_id) {
            sub_ids.iter().cloned().collect()
        } else {Vec::<u64>::new()};
        for subs_id in suber_subs {
            self.remove_subscription(subs_id)
        }
        self.subscribers.remove(&suber_id);
        self.suber_subs.remove(&suber_id);
    }

    pub fn remove_subscription(&mut self, id: u64) {
        let sub_key = self.sub_to_key.remove(&id);
        if let Some(sub_key) = sub_key {
            if let Some(ref mut sub_subers) = self.subscriptions.get_mut(&sub_key) {
                sub_subers.remove(&id);
                self.sub_suber.remove(&id);
            }
        }
    }
}

pub struct SMCallback {
    pub subscriptions: Arc<RwLock<Subscriptions>>,
    pub raft_service: Arc<RaftService>,
    pub sm_id: u64,
}

impl SMCallback {
    pub fn new(state_machine_id: u64, raft_service: Arc<RaftService>) -> SMCallback {
        let meta = raft_service.meta.read();
        let sm = meta.state_machine.read();
        let subs = sm.configs.subscriptions.clone();
        SMCallback {
            subscriptions: subs,
            raft_service: raft_service.clone(),
            sm_id: state_machine_id,
        }
    }
    pub fn notify<R>(&self, func: &RaftMsg<R>, data: R)
    where R: serde::Serialize + Send + Sync {
        if !IS_LEADER.get() {return;}
        let (fn_id, op_type, pattern_data) = func.encode();
        match op_type {
            OpType::SUBSCRIBE => {
                let pattern_id = hash_bytes(&pattern_data.as_slice());
                let raft_sid = self.raft_service.options.service_id;
                let sm_id = self.sm_id;
                let key = (raft_sid, sm_id, fn_id, pattern_id);
                let svr_subs = self.subscriptions.read();
                if let Some(sub_ids) = svr_subs.subscriptions.get(&key) {
                    let data = bincode::serialize(&data);
                    for sub_id in sub_ids {
                        if let Some(subscriber_id) = svr_subs.sub_suber.get(&sub_id) {
                            if let Some(subscriber) = svr_subs.subscribers.get(&subscriber_id) {
                                let data = data.clone();
                                let client = subscriber.client.clone();
                                THREAD_POOL.lock().execute(move || {
                                    client.notify(&key, &data);
                                });
                            }
                        }
                    }
                }
            },
            _ => {
                panic!("Cannot notify on {:?}")
            }
        }
    }
}

pub fn notify<R, F>(callback: &Option<SMCallback>, func: &RaftMsg<R>, data: F)
    where F: Fn() -> R,
          R: serde::Serialize + Send + Sync {
    if let Some(ref callback) = *callback {
        callback.notify(func, data());
    }
}