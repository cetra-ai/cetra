// SPDX-License-Identifier: Elastic-2.0
//! Pending SAML AuthnRequests (D-SA-10): in memory, single use, bound to the browser.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::IpAddr;

use cetra_core::ids::OrgId;
use sha2::{Digest, Sha256};

use crate::auth::secret::constant_time_eq;

pub(crate) const PENDING_TTL_NS: i64 = 600_000_000_000;
pub(crate) const MAX_PER_IP: usize = 50;
pub(crate) const MAX_PER_ORG: usize = 1_000;
pub(crate) const MAX_TOTAL: usize = 100_000;

/// One pending login. A manual `Debug` prints `org`, whether `test_owner` is set and
/// `expires_ns` only, hiding the redirect, the binder hash and the IP.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) org: OrgId,
    pub(crate) test_owner: Option<uuid::Uuid>,
    pub(crate) redirect: String,
    /// Lowercase hex SHA-256 of the binder cookie's value.
    pub(crate) binder_sha256: String,
    /// `crate::auth::limits::network_key(client ip)`: IPv6 counted by its /64.
    pub(crate) network: IpAddr,
    pub(crate) expires_ns: i64,
}

impl fmt::Debug for Pending {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Pending")
            .field("org", &self.org)
            .field("test_owner", &self.test_owner.is_some())
            .field("expires_ns", &self.expires_ns)
            .finish_non_exhaustive()
    }
}

/// Why `take` found no usable entry. Both answer `saml_invalid`; the distinction is for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TakeError {
    Unknown,
    Binder,
}

/// A manual `Debug` prints the entry count only. Every index is kept in step with `entries`
/// (eager removal), so no operation scans the whole table under the lock.
#[derive(Default)]
pub(crate) struct PendingTable {
    entries: HashMap<String, Pending>,
    /// Insertion (= expiry) order; may hold ids already removed (skipped lazily).
    order: VecDeque<String>,
    /// Ids per network, oldest first (at most `MAX_PER_IP` each).
    by_network: HashMap<IpAddr, VecDeque<String>>,
    /// Ids per org and network, oldest first.
    by_org: HashMap<OrgId, HashMap<IpAddr, VecDeque<String>>>,
    /// Live entries per org.
    org_counts: HashMap<OrgId, usize>,
}

impl fmt::Debug for PendingTable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingTable")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl PendingTable {
    /// Records `request_id`, evicting expired entries, then the oldest entry of the same
    /// network while it is at its limit, then (org full) the oldest entry of the network that
    /// holds the most entries of that org, then the oldest overall at `MAX_TOTAL`.
    pub(crate) fn insert(&mut self, request_id: String, entry: Pending, now: i64) {
        // Step 1: expired or already removed ids at the front of the insertion order.
        while let Some(front) = self.order.front().cloned() {
            let expired = self
                .entries
                .get(&front)
                .map(|pending| pending.expires_ns <= now);
            match expired {
                None => {
                    self.order.pop_front();
                }
                Some(true) => {
                    self.order.pop_front();
                    self.remove(&front);
                }
                Some(false) => break,
            }
        }

        // Step 2: the network is at its limit.
        let network_full = self
            .by_network
            .get(&entry.network)
            .is_some_and(|queue| queue.len() >= MAX_PER_IP);
        if network_full
            && let Some(oldest) = self
                .by_network
                .get(&entry.network)
                .and_then(|queue| queue.front())
                .cloned()
        {
            self.remove(&oldest);
        }

        // Step 3: the org is full. Evict from the network that holds the most of its entries.
        let org_full = self
            .org_counts
            .get(&entry.org)
            .is_some_and(|count| *count >= MAX_PER_ORG);
        if org_full {
            let mut largest: Option<(IpAddr, usize)> = None;
            if let Some(networks) = self.by_org.get(&entry.org) {
                for (network, queue) in networks {
                    if largest.is_none_or(|(_, length)| queue.len() > length) {
                        largest = Some((*network, queue.len()));
                    }
                }
            }
            if let Some((network, _)) = largest
                && let Some(oldest) = self
                    .by_org
                    .get(&entry.org)
                    .and_then(|networks| networks.get(&network))
                    .and_then(|queue| queue.front())
                    .cloned()
            {
                self.remove(&oldest);
            }
        }

        // Step 4: the table is full. Evict the oldest entry overall.
        while self.entries.len() >= MAX_TOTAL {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.remove(&oldest);
        }

        // Step 5: index and store the new entry.
        let (org, network) = (entry.org, entry.network);
        self.order.push_back(request_id.clone());
        self.by_network
            .entry(network)
            .or_default()
            .push_back(request_id.clone());
        self.by_org
            .entry(org)
            .or_default()
            .entry(network)
            .or_default()
            .push_back(request_id.clone());
        *self.org_counts.entry(org).or_insert(0) += 1;
        self.entries.insert(request_id, entry);
    }

    /// The entry for `request_id` when it exists, is not expired, belongs to `org`, and
    /// `binder`'s SHA-256 matches (constant-time). It is removed only on success; a wrong
    /// binder leaves it in place (D-SA-10). An expired entry is removed and is `Unknown`.
    pub(crate) fn take(
        &mut self,
        request_id: &str,
        org: OrgId,
        binder: Option<&str>,
        now: i64,
    ) -> Result<Pending, TakeError> {
        let (expires_ns, entry_org, expected) = match self.entries.get(request_id) {
            Some(entry) => (entry.expires_ns, entry.org, entry.binder_sha256.clone()),
            None => return Err(TakeError::Unknown),
        };
        if expires_ns <= now {
            self.remove(request_id);
            return Err(TakeError::Unknown);
        }
        if entry_org != org {
            return Err(TakeError::Unknown);
        }
        let Some(binder) = binder else {
            return Err(TakeError::Binder);
        };
        let digest = hex::encode(Sha256::digest(binder.as_bytes()));
        if !constant_time_eq(&digest, &expected) {
            return Err(TakeError::Binder);
        }
        self.remove(request_id).ok_or(TakeError::Unknown)
    }

    /// Number of live entries.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Removes `id` from the store and from every index. `order` keeps it until `insert`
    /// skips it.
    fn remove(&mut self, id: &str) -> Option<Pending> {
        let entry = self.entries.remove(id)?;
        let mut network_empty = false;
        if let Some(queue) = self.by_network.get_mut(&entry.network) {
            if let Some(position) = queue.iter().position(|item| item == id) {
                queue.remove(position);
            }
            network_empty = queue.is_empty();
        }
        if network_empty {
            self.by_network.remove(&entry.network);
        }
        let mut org_empty = false;
        if let Some(networks) = self.by_org.get_mut(&entry.org) {
            let mut org_network_empty = false;
            if let Some(queue) = networks.get_mut(&entry.network) {
                if let Some(position) = queue.iter().position(|item| item == id) {
                    queue.remove(position);
                }
                org_network_empty = queue.is_empty();
            }
            if org_network_empty {
                networks.remove(&entry.network);
            }
            org_empty = networks.is_empty();
        }
        if org_empty {
            self.by_org.remove(&entry.org);
        }
        let emptied = self.org_counts.get_mut(&entry.org).is_some_and(|count| {
            *count = count.saturating_sub(1);
            *count == 0
        });
        if emptied {
            self.org_counts.remove(&entry.org);
        }
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    const BINDER: &str = "binder-value";

    fn binder_hash(binder: &str) -> String {
        hex::encode(Sha256::digest(binder.as_bytes()))
    }

    fn pending(org: OrgId, network: IpAddr, expires_ns: i64) -> Pending {
        Pending {
            org,
            test_owner: None,
            redirect: "/acme".to_owned(),
            binder_sha256: binder_hash(BINDER),
            network,
            expires_ns,
        }
    }

    fn v4(a: u8, b: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, a, b))
    }

    #[test]
    fn an_entry_is_taken_once() {
        let mut table = PendingTable::default();
        let org = OrgId::new_v7();
        table.insert("r1".to_owned(), pending(org, v4(0, 1), 1_000), 0);
        assert_eq!(table.len(), 1);
        let taken = table.take("r1", org, Some(BINDER), 10).expect("taken");
        assert_eq!(taken.org, org);
        assert_eq!(table.len(), 0);
        assert_eq!(
            table.take("r1", org, Some(BINDER), 10),
            Err(TakeError::Unknown)
        );
    }

    #[test]
    fn a_wrong_binder_keeps_the_entry() {
        let mut table = PendingTable::default();
        let org = OrgId::new_v7();
        table.insert("r1".to_owned(), pending(org, v4(0, 1), 1_000), 0);
        assert_eq!(
            table.take("r1", org, Some("other"), 10),
            Err(TakeError::Binder)
        );
        assert_eq!(table.len(), 1);
        assert!(table.take("r1", org, Some(BINDER), 10).is_ok());
    }

    #[test]
    fn a_missing_binder_keeps_the_entry() {
        let mut table = PendingTable::default();
        let org = OrgId::new_v7();
        table.insert("r1".to_owned(), pending(org, v4(0, 1), 1_000), 0);
        assert_eq!(table.take("r1", org, None, 10), Err(TakeError::Binder));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn another_org_cannot_take_it() {
        let mut table = PendingTable::default();
        let org = OrgId::new_v7();
        table.insert("r1".to_owned(), pending(org, v4(0, 1), 1_000), 0);
        assert_eq!(
            table.take("r1", OrgId::new_v7(), Some(BINDER), 10),
            Err(TakeError::Unknown)
        );
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn an_expired_entry_is_unknown_and_removed() {
        let mut table = PendingTable::default();
        let org = OrgId::new_v7();
        table.insert("r1".to_owned(), pending(org, v4(0, 1), 100), 0);
        assert_eq!(
            table.take("r1", org, Some(BINDER), 100),
            Err(TakeError::Unknown)
        );
        assert_eq!(table.len(), 0);
        assert!(table.by_network.is_empty());
        assert!(table.by_org.is_empty());
        assert!(table.org_counts.is_empty());
    }

    #[test]
    fn the_51st_entry_of_a_network_evicts_its_oldest() {
        let mut table = PendingTable::default();
        let network = v4(0, 1);
        for index in 0..MAX_PER_IP {
            let org = OrgId::new_v7();
            table.insert(
                format!("r{index}"),
                pending(org, network, i64::MAX),
                index as i64,
            );
        }
        assert_eq!(table.len(), MAX_PER_IP);
        table.insert(
            "r-new".to_owned(),
            pending(OrgId::new_v7(), network, i64::MAX),
            100,
        );
        assert_eq!(table.len(), MAX_PER_IP);
        assert!(!table.entries.contains_key("r0"));
        assert!(table.entries.contains_key("r1"));
        assert!(table.entries.contains_key("r-new"));
        assert_eq!(table.by_network[&network].len(), MAX_PER_IP);
    }

    #[test]
    fn a_full_org_evicts_from_its_largest_network() {
        // One org of 1,000 entries: network 10.0.0.1 holds 50, networks 10.0.2.1 to
        // 10.0.21.1 hold 47 each, and 10.0.22.1 holds 10. The largest is unique.
        let mut table = PendingTable::default();
        let org = OrgId::new_v7();
        let mut counter = 0;
        let mut fill = |table: &mut PendingTable, network: IpAddr, count: usize| {
            for _ in 0..count {
                counter += 1;
                table.insert(
                    format!("r{counter}"),
                    pending(org, network, i64::MAX),
                    counter,
                );
            }
        };
        fill(&mut table, v4(0, 1), MAX_PER_IP);
        for octet in 2..=21 {
            fill(&mut table, v4(0, octet), 47);
        }
        fill(&mut table, v4(0, 22), 10);
        assert_eq!(table.len(), MAX_PER_ORG);

        fill(&mut table, v4(0, 23), 1);
        assert_eq!(table.len(), MAX_PER_ORG);
        assert!(
            !table.entries.contains_key("r1"),
            "oldest of the largest network"
        );
        assert_eq!(table.by_org[&org][&v4(0, 1)].len(), MAX_PER_IP - 1);
        assert_eq!(table.org_counts[&org], MAX_PER_ORG);
    }

    #[test]
    fn other_networks_keep_their_entries() {
        let mut table = PendingTable::default();
        let org = OrgId::new_v7();
        // The only entry of network 10.0.9.9 is the oldest overall, but it is not in the
        // largest network, so it must stay.
        table.insert("lonely".to_owned(), pending(org, v4(9, 9), i64::MAX), 0);
        let mut counter = 0;
        for _ in 0..MAX_PER_IP {
            counter += 1;
            table.insert(
                format!("big{counter}"),
                pending(org, v4(0, 1), i64::MAX),
                counter,
            );
        }
        for octet in 2..=20 {
            for _ in 0..MAX_PER_IP - 1 {
                counter += 1;
                table.insert(
                    format!("big{counter}"),
                    pending(org, v4(0, octet), i64::MAX),
                    counter,
                );
            }
        }
        // 1 + 50 + 19 * 49 = 982 entries, and 18 more fill the org to MAX_PER_ORG.
        for _ in 0..(MAX_PER_ORG - table.len()) {
            counter += 1;
            table.insert(
                format!("fill{counter}"),
                pending(org, v4(0, 30), i64::MAX),
                counter,
            );
        }
        assert_eq!(table.len(), MAX_PER_ORG);
        table.insert(
            "newcomer".to_owned(),
            pending(org, v4(0, 31), i64::MAX),
            counter + 1,
        );
        assert!(table.entries.contains_key("lonely"));
        assert_eq!(table.len(), MAX_PER_ORG);
    }

    #[test]
    fn debug_hides_the_binder_hash_and_redirect() {
        let org = OrgId::new_v7();
        let entry = pending(org, v4(0, 1), 42);
        let text = format!("{entry:?}");
        assert!(!text.contains(&entry.binder_sha256), "{text}");
        assert!(!text.contains("/acme"), "{text}");
        assert!(text.contains("42"), "{text}");

        let mut table = PendingTable::default();
        table.insert("r1".to_owned(), entry, 0);
        assert_eq!(format!("{table:?}"), "PendingTable { entries: 1 }");
    }
}
