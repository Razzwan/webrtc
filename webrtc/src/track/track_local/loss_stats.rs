use std::{collections::HashMap, time::Instant};

#[derive(Debug)]
pub struct SingleReceiverStats {
    fraction_lost: u8,
    packets_lost: i32,
    last_report_time: Instant,
    report_count: u32,
}

#[derive(Debug)]
pub struct ReceiverLossStats {
    pub average_fraction_lost: u8,                     // Средние потери
    pub worst_fraction_lost: u8,                       // Наихудшие потери среди всех
    receiver_stats: HashMap<u32, SingleReceiverStats>, // Key: SSRC получателя
    last_update: Instant,
}

pub enum ReduceIncrease {
    Reduce(u32),
    Increase,
    NoChange,
}

impl ReceiverLossStats {
    pub fn new() -> Self {
        Self {
            receiver_stats: HashMap::new(),
            worst_fraction_lost: 0,
            average_fraction_lost: 0,
            last_update: Instant::now(),
        }
    }

    pub fn update_with_rr(&mut self, receiver_ssrc: u32, fraction_lost: u8, packets_lost: i32) {
        // Обновляем статистику для конкретного получателя
        let receiver_stat =
            self.receiver_stats
                .entry(receiver_ssrc)
                .or_insert_with(|| SingleReceiverStats {
                    fraction_lost: 0,
                    packets_lost: 0,
                    last_report_time: Instant::now(),
                    report_count: 0,
                });

        receiver_stat.fraction_lost = fraction_lost;
        receiver_stat.packets_lost = packets_lost;
        receiver_stat.last_report_time = Instant::now();
        receiver_stat.report_count += 1;

        // Пересчитываем агрегированную статистику
        self.recalculate_aggregates();
        self.last_update = Instant::now();
    }

    fn recalculate_aggregates(&mut self) {
        if self.receiver_stats.is_empty() {
            self.worst_fraction_lost = 0;
            self.average_fraction_lost = 0;
            return;
        }

        // Находим наихудшие потери
        self.worst_fraction_lost = self
            .receiver_stats
            .values()
            .map(|stats| stats.fraction_lost)
            .max()
            .unwrap_or(0);

        // Вычисляем средние потери
        let sum: u32 = self
            .receiver_stats
            .values()
            .map(|stats| stats.fraction_lost as u32)
            .sum();
        self.average_fraction_lost = (sum / self.receiver_stats.len() as u32) as u8;
    }

    pub fn should_reduce(&self) -> ReduceIncrease {
        if self.worst_fraction_lost > 10 {
            ReduceIncrease::Reduce(std::cmp::max(20, self.average_fraction_lost as u32))
        } else if self.worst_fraction_lost <= 2 {
            ReduceIncrease::Increase
        } else {
            ReduceIncrease::NoChange
        }
    }
}
