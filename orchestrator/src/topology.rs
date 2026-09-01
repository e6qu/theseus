// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A stable service-order scheduler for one in-process Theseus topology.
//!
//! Firecracker's regular process runner leaves device events to host epoll.
//! A topology must instead make simulated-network delivery explicit: every
//! scheduling round pumps services in lexical service-name order. The shared
//! [`theseus_engine::simnet::SimSwitch`] preserves frame order within that
//! round, so the runner can record and replay a topology without host sockets.

use std::collections::BTreeMap;

/// An operation performed by one guest during a scheduler round.
pub trait TopologyService {
    /// Pump pending host-independent device work for this service.
    fn pump(&mut self);
}

/// A deterministic, single-threaded topology scheduler.
#[derive(Debug)]
pub struct TopologyScheduler<T> {
    services: BTreeMap<String, T>,
    rounds: u64,
}

impl<T> TopologyScheduler<T> {
    /// Create a scheduler. Empty or duplicate names are rejected so the run
    /// order is an unambiguous part of the replay contract.
    pub fn new(services: impl IntoIterator<Item = (String, T)>) -> Result<Self, String> {
        let mut ordered = BTreeMap::new();
        for (name, service) in services {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                return Err(format!("invalid service name {name:?}"));
            }
            if ordered.insert(name.clone(), service).is_some() {
                return Err(format!("duplicate service name {name:?}"));
            }
        }
        if ordered.is_empty() {
            return Err("a topology needs at least one service".to_owned());
        }
        Ok(Self {
            services: ordered,
            rounds: 0,
        })
    }

    /// Pump every service once in stable lexical order.
    pub fn pump_round(&mut self)
    where
        T: TopologyService,
    {
        for service in self.services.values_mut() {
            service.pump();
        }
        self.rounds += 1;
    }

    /// Stable service names included in a replay plan.
    pub fn service_names(&self) -> Vec<String> {
        self.services.keys().cloned().collect()
    }

    /// Number of completed deterministic rounds.
    pub fn rounds(&self) -> u64 {
        self.rounds
    }

    /// Access a service by its Compose name.
    pub fn service_mut(&mut self, name: &str) -> Option<&mut T> {
        self.services.get_mut(name)
    }

    /// Consume the scheduler after the run is complete.
    pub fn into_services(self) -> BTreeMap<String, T> {
        self.services
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Debug)]
    struct Service {
        name: String,
        trace: Arc<Mutex<Vec<String>>>,
    }

    impl TopologyService for Service {
        fn pump(&mut self) {
            self.trace.lock().unwrap().push(self.name.clone());
        }
    }

    #[test]
    fn pumps_services_in_stable_name_order_every_round() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let mut scheduler = TopologyScheduler::new([
            (
                "worker".to_owned(),
                Service {
                    name: "worker".to_owned(),
                    trace: trace.clone(),
                },
            ),
            (
                "api".to_owned(),
                Service {
                    name: "api".to_owned(),
                    trace: trace.clone(),
                },
            ),
        ])
        .unwrap();
        scheduler.pump_round();
        scheduler.pump_round();
        assert_eq!(scheduler.service_names(), ["api", "worker"]);
        assert_eq!(scheduler.rounds(), 2);
        assert_eq!(*trace.lock().unwrap(), ["api", "worker", "api", "worker"]);
    }

    #[test]
    fn rejects_ambiguous_service_names() {
        assert!(TopologyScheduler::<()>::new([]).is_err());
        assert!(TopologyScheduler::new([("api/name".to_owned(), ())]).is_err());
        assert!(TopologyScheduler::new([("api".to_owned(), ()), ("api".to_owned(), ())]).is_err());
    }
}
