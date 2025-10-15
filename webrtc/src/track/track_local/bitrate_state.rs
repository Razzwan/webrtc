use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct BitrateSample {
    timestamp: Instant,
    bitrate_bps: u32,
}

const BITRATE_HISTORY_SIZE: usize = 15; // 5 сек / 333 мс ≈ 15 samples

#[derive(Debug, Clone)]
pub struct BitrateState {
    last_octet_count: u32,
    last_timestamp: Option<std::time::Instant>,
    /// Текущий битрейт измеряется в битах в секунду
    current_bitrate_bps: u32,

    // Кольцевой буфер фиксированного размера.
    // Несколько записанных изменений подряд. Используются для расчёта среднего битрейта
    history: [BitrateSample; BITRATE_HISTORY_SIZE],
    start_index: usize, // Индекс первого валидного элемента
    count: usize,       // Количество валидных элементов
    /// Для отчётов RR для адаптивного битрейта лучше использовать 3 секунды
    max_history_duration: Duration,
}

impl BitrateState {
    pub fn new() -> Self {
        let default_sample = BitrateSample {
            timestamp: Instant::now(),
            bitrate_bps: 0,
        };
        Self {
            last_octet_count: 0,
            last_timestamp: None,
            current_bitrate_bps: 0,
            history: [default_sample; BITRATE_HISTORY_SIZE],
            start_index: 0,
            count: 0,
            max_history_duration: Duration::from_secs(5),
        }
    }

    pub fn with_max_duration(mut self, duration: Duration) -> Self {
        self.max_history_duration = duration;
        self
    }

    pub fn current_bitrate(&self) -> u32 {
        self.current_bitrate_bps
    }

    pub fn update_with_sr(&mut self, octet_count: u32) {
        let now = Instant::now();

        if let Some(last_ts) = self.last_timestamp {
            let time_diff = now.duration_since(last_ts).as_secs_f64();

            if time_diff > 0.0 {
                let octet_diff = octet_count.wrapping_sub(self.last_octet_count);
                self.current_bitrate_bps = (octet_diff as f64 * 8.0 / time_diff) as u32;

                // Добавляем текущее измерение в историю
                self.add_sample(now, self.current_bitrate_bps);
            }
        }

        self.last_octet_count = octet_count;
        self.last_timestamp = Some(now);
    }

    fn add_sample(&mut self, timestamp: Instant, bitrate_bps: u32) {
        let cutoff_time = timestamp - self.max_history_duration;

        // Очищаем устаревшие samples (двигаем start_index)
        while self.count > 0 {
            let oldest_sample = &self.history[self.start_index];
            if oldest_sample.timestamp < cutoff_time {
                self.start_index = (self.start_index + 1) % BITRATE_HISTORY_SIZE;
                self.count -= 1;
            } else {
                break;
            }
        }

        // Добавляем новый sample
        let insert_index = (self.start_index + self.count) % BITRATE_HISTORY_SIZE;
        self.history[insert_index] = BitrateSample {
            timestamp,
            bitrate_bps,
        };

        if self.count < BITRATE_HISTORY_SIZE {
            self.count += 1;
        } else {
            // Буфер полный - перезаписываем самый старый (двигаем start_index)
            self.start_index = (self.start_index + 1) % BITRATE_HISTORY_SIZE;
        }
    }

    pub fn average_bitrate(&self) -> u32 {
        if self.count == 0 {
            return 0;
        }

        let mut sum: u64 = 0;
        for i in 0..self.count {
            let index = (self.start_index + i) % BITRATE_HISTORY_SIZE;
            sum += self.history[index].bitrate_bps as u64;
        }

        (sum / self.count as u64) as u32
    }
}
