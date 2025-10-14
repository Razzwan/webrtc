use arc_swap::ArcSwapOption;
use crossbeam_epoch::{self as epoch, Atomic, Owned};
use crossbeam_utils::CachePadded;
use log::warn;
use rtp::packet::Packet;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct PCache {
    pub rtp: Packet,            // готовый RTP-пакет для ретрансляции
    pub first_sent_at: Instant, // для TTL
}

#[derive(Debug)]
pub struct PCacheBuffer {
    ttl: Duration,
    mask: usize, // capacity_pow2 - 1
    slots: Vec<CachePadded<ArcSwapOption<PCache>>>,
}

impl PCacheBuffer {
    pub fn new(ttl: Duration, capacity_pow2: usize) -> Self {
        assert!(capacity_pow2.is_power_of_two());
        let slots = (0..capacity_pow2)
            .map(|_| CachePadded::new(ArcSwapOption::from(None)))
            .collect();
        Self {
            ttl,
            mask: capacity_pow2 - 1,
            slots,
        }
    }

    #[inline]
    fn idx(&self, seq: u16) -> usize {
        (seq as usize) & self.mask
    }

    pub fn put(&self, entry: Arc<PCache>) {
        let idx = self.idx(entry.rtp.header.sequence_number);
        self.slots[idx].store(Some(entry));
    }

    // Нулевая копия: лучше отдавать Arc<PCache>, а не Packet
    pub fn get_arc(&self, seq: u16) -> Option<Arc<PCache>> {
        let cell = &self.slots[self.idx(seq)];

        match cell.load_full() {
            Some(entry) => {
                if entry.rtp.header.sequence_number != seq {
                    warn!(
                        "Коллизия кольца entry.seq {} != seq{}",
                        entry.rtp.header.sequence_number, seq
                    );
                    return None;
                }
                if entry.first_sent_at.elapsed() > self.ttl {
                    warn!(
                        "Пакет просрочен {:?} > {:?}",
                        entry.first_sent_at.elapsed(),
                        self.ttl
                    );
                    return None;
                }
                Some(entry)
            }
            None => {
                warn!("LoadFull is None");
                None
            }
        }
    }

    // Совместимо с текущим API: отдаём Packet по значению (клон)
    pub fn get(&self, seq: u16) -> Option<Packet> {
        self.get_arc(seq).map(|e| e.rtp.clone())
    }
}

#[derive(Debug)]
pub struct PCache2 {
    pub rtp: Packet,            // полный RTP (ingress)
    pub first_sent_at: Instant, // для TTL
}

#[derive(Debug)]
pub struct PCacheBuffer2 {
    ttl: Duration,
    mask: usize, // capacity_pow2 - 1
    slots: Vec<CachePadded<Atomic<PCache>>>,
}

impl PCacheBuffer2 {
    pub fn new(ttl: Duration, capacity_pow2: usize) -> Self {
        assert!(capacity_pow2.is_power_of_two());
        let slots = (0..capacity_pow2)
            .map(|_| CachePadded::new(Atomic::null()))
            .collect();
        Self {
            ttl,
            mask: capacity_pow2 - 1,
            slots,
        }
    }

    #[inline(always)]
    fn idx(&self, seq: u16) -> usize {
        (seq as usize) & self.mask
    }

    // Один писатель: атомарная замена указателя, без refcount
    pub fn put(&self, rtp: Packet) {
        let idx = self.idx(rtp.header.sequence_number);
        let guard = &epoch::pin();

        let new = Owned::new(PCache {
            rtp,
            first_sent_at: Instant::now(),
        });

        // swap возвращает старый Shared
        let old = self.slots[idx].swap(new, std::sync::atomic::Ordering::Release, guard);

        // Безопасно отложим уничтожение старого
        if !old.is_null() {
            unsafe { guard.defer_destroy(old) };
        }
    }

    pub fn get(&self, seq: u16) -> Option<Packet> {
        let idx = self.idx(seq);
        let guard = &epoch::pin();

        let shared = self.slots[idx].load(std::sync::atomic::Ordering::Acquire, guard);
        let pc = unsafe { shared.as_ref()? };

        // Защита от коллизий кольца
        if pc.rtp.header.sequence_number != seq {
            return None;
        }
        // TTL
        if pc.first_sent_at.elapsed() > self.ttl {
            return None;
        }
        Some(pc.rtp.clone())
    }
}

#[cfg(test)]
mod packet_cache_test {
    use super::*;
    use bytes::Bytes;
    use rtp::{
        header::{Extension, Header},
        packet::Packet,
    };
    use std::time::Duration;

    fn get_packet(sequence_number: u16) -> Packet {
        Packet {
            header: Header {
                version: 2,
                padding: false,
                extension: true,
                marker: true,
                payload_type: 96,
                sequence_number,
                timestamp: 10000000,
                ssrc: 100030001,
                csrc: vec![],
                extension_profile: 1,
                extensions: vec![Extension {
                    id: 0,
                    payload: Bytes::from_static(&[0xFF, 0xFF, 0xFF, 0xFF]),
                }],
                ..Default::default()
            },
            payload: Bytes::from_static(&[0x98, 0x36, 0xbe, 0x88, 0x9e]),
        }
    }

    fn get_p_cache(sequence_number: u16) -> Arc<PCache> {
        Arc::new(PCache {
            rtp: get_packet(sequence_number),
            first_sent_at: Instant::now(),
        })
    }

    #[test]
    fn it_packet_cache1() {
        let cache_buf = PCacheBuffer::new(Duration::from_millis(3000), 2);

        let p1 = get_p_cache(1);
        let p2 = get_p_cache(2);
        let p3 = get_p_cache(3);

        cache_buf.put(p1.clone());
        cache_buf.put(p2.clone());
        cache_buf.put(p3.clone());
        if let Some(p) = cache_buf.get(3) {
            assert_eq!(p.header.sequence_number, 3, "Неверный seq_num");
        } else {
            assert!(false, "Нет пакета 3");
        }

        if let Some(p) = cache_buf.get(2) {
            assert_eq!(p.header.sequence_number, 2, "Неверный seq_num");
        } else {
            assert!(false, "Нет пакета 2");
        }

        if let Some(p) = cache_buf.get(1) {
            assert!(
                false,
                "Найдет пакет 1, которого быть не должно, в нём seq={}",
                p.header.sequence_number
            );
        }
    }

    #[test]
    fn it_packet_cache2() {
        let cache_buf = PCacheBuffer2::new(Duration::from_millis(3000), 2);

        let p1 = get_packet(1);
        let p2 = get_packet(2);
        let p3 = get_packet(3);

        cache_buf.put(p1.clone());
        cache_buf.put(p2.clone());
        cache_buf.put(p3.clone());
        if let Some(p) = cache_buf.get(3) {
            assert_eq!(p.header.sequence_number, 3, "Неверный seq_num");
        } else {
            assert!(false, "Нет пакета 3");
        }

        if let Some(p) = cache_buf.get(2) {
            assert_eq!(p.header.sequence_number, 2, "Неверный seq_num");
        } else {
            assert!(false, "Нет пакета 2");
        }

        if let Some(p) = cache_buf.get(1) {
            assert!(
                false,
                "Найдет пакет 1, которого быть не должно, в нём seq={}",
                p.header.sequence_number
            );
        }
    }
}
