use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use rtcp::reception_report::ReceptionReport;
use std::any::Any;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::time::Instant;
use std::{borrow::Cow, collections::HashMap, time::Duration};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::Mutex;
use util::{Marshal, MarshalSize};

use super::*;
use crate::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8};
use crate::track::track_local::bitrate_state::BitrateState;
use crate::track::track_local::loss_stats::{ReceiverLossStats, ReduceIncrease};
use crate::track::track_local::packet_cache::PCache;
use crate::track::track_remote::TrackRemote;
use crate::{error::flatten_errs, track::track_local::packet_cache::PCacheBuffer};

#[derive(Clone, Debug)]
pub struct TrackState {
    last_out_seq: u16, // переживает все переключения источников
    last_out_ts: u32,  // переживает все переключения источников
    started_at_ts: i64,
    marker: bool, // marker = true означает, что это последний пакет видеофрейма.
    // Это сигнал для джиттер-буфера и декодера, что можно собрать все полученные пакеты этого фрейма и отправить их на декодирование.
    // Используется после паузы
    out_offset: Option<(
        u16, /* смещение порядкового номера */
        u32, /* смещение временной метки timestamp */
    )>,
}

pub struct PkgAttrs {
    sequence_number: u16,
    timestamp: u32,
    marker: bool,
}

impl TrackState {
    pub fn new() -> Self {
        TrackState {
            // Порядковый номер начинается с 0
            last_out_seq: 0,
            // время трека начинается с 0
            last_out_ts: 0,
            // Сохраняем начало трека в реальной временной шкале дла последующей синхронизации
            started_at_ts: chrono::Utc::now().timestamp(),
            out_offset: None,
            marker: false,
        }
    }

    pub fn get_pkg_attrs(
        &mut self,
        kind: RTPCodecType,
        pkt_sequence_number: u16,
        pkt_timestamp: u32,
    ) -> PkgAttrs {
        match self.out_offset {
            Some((seq_num_offset, ts_offset)) => PkgAttrs {
                sequence_number: pkt_sequence_number.wrapping_add(seq_num_offset),
                timestamp: pkt_timestamp.wrapping_add(ts_offset),
                marker: self.marker,
            },
            None => {
                let seq_num_offset = self
                    .last_out_seq
                    .wrapping_sub(pkt_sequence_number)
                    .wrapping_add(1);
                let ts_offset =
                    self.last_out_ts
                        .wrapping_sub(pkt_timestamp)
                        .wrapping_add(match kind {
                            RTPCodecType::Audio => 900,  // стандартное значение для звука
                            RTPCodecType::Video => 3750, // 90000 clock_rate / 24 кадра
                            _ => 3750,
                        });
                self.out_offset = Some((seq_num_offset, ts_offset));

                PkgAttrs {
                    sequence_number: pkt_sequence_number.wrapping_add(seq_num_offset),
                    timestamp: pkt_timestamp.wrapping_add(ts_offset),
                    marker: self.marker,
                }
            }
        }
    }

    pub fn set_last_out(&mut self, pkg_attrs: PkgAttrs) {
        self.last_out_seq = pkg_attrs.sequence_number;
        self.last_out_ts = pkg_attrs.timestamp;
        self.marker = false;
    }

    pub fn shift_offset(&mut self, pkt_sequence_number: u16, pkt_timestamp: u32) {
        self.marker = true;
        self.out_offset = Some((
            self.last_out_seq.wrapping_sub(pkt_sequence_number),
            self.last_out_ts.wrapping_sub(pkt_timestamp),
        ))
    }

    pub fn get_pkg_attrs_set_last_out(
        &mut self,
        kind: RTPCodecType,
        pkt_sequence_number: u16,
        pkt_timestamp: u32,
    ) -> PkgAttrs {
        match self.out_offset {
            Some((seq_num_offset, ts_offset)) => {
                // новый = пришедший + смещение
                // старый = новый - (новый - старый)
                self.last_out_seq = pkt_sequence_number.wrapping_add(seq_num_offset);
                self.last_out_ts = pkt_timestamp.wrapping_add(ts_offset);
                PkgAttrs {
                    sequence_number: self.last_out_seq,
                    timestamp: self.last_out_ts,
                    marker: self.marker,
                }
            }
            None => {
                let seq_num_offset = self
                    .last_out_seq
                    .wrapping_sub(pkt_sequence_number)
                    .wrapping_add(1);
                let ts_offset =
                    self.last_out_ts
                        .wrapping_sub(pkt_timestamp)
                        .wrapping_add(match kind {
                            RTPCodecType::Audio => 900,  // стандартное значение для звука
                            RTPCodecType::Video => 6000, // 90000 clock_rate / 24 кадра
                            _ => 6000,
                        });
                self.out_offset = Some((seq_num_offset, ts_offset));

                self.last_out_seq = pkt_sequence_number.wrapping_add(seq_num_offset);
                self.last_out_ts = pkt_timestamp.wrapping_add(ts_offset);

                // println!(
                //     "Смещения перезаписаны seq_num: {pkt_sequence_number} -> {}; ts: {pkt_timestamp} -> {}",
                //     self.last_out_seq, self.last_out_ts
                // );
                PkgAttrs {
                    sequence_number: self.last_out_seq,
                    timestamp: self.last_out_ts,
                    marker: self.marker,
                }
            }
        }
    }

    pub fn origin_seq(&self, modified_seq: u16) -> u16 {
        match self.out_offset {
            Some((seq_num_offset, _)) => modified_seq.wrapping_sub(seq_num_offset),
            None => modified_seq,
        }
    }
}

/// TrackLocalStaticRTP  is a TrackLocal that has a pre-set codec and accepts RTP Packets.
/// If you wish to send a media.Sample use TrackLocalStaticSample
#[derive(Debug)]
pub struct TrackLocalStaticRTP {
    pub(crate) bindings: DashMap<u32, Arc<TrackBinding>>,
    codec: RTCRtpCodecCapability,
    id: String,
    rid: Option<String>,
    stream_id: String,

    pub state: Mutex<TrackState>,
    pub rtp_cache: Arc<PCacheBuffer>,

    pli_last_ms: AtomicU64,
    pli_interval_ms: u64,

    pub bitrate: Mutex<BitrateState>,
    pub loss_stats: Mutex<ReceiverLossStats>,

    pub max_bitrate_bps: u32,
    pub current_target_bitrate_bps: AtomicU32, // Добавляем это поле

    last_remb_sent: AtomicU64, // microseconds since start
    start_remb_time: Instant,  // Референсное время
    min_remb_interval: Duration,
}

/// Количество пакетов в кэше
const CAPACITY: usize = 256; // если 24 пакета в секунду, то на 3 секунды нужно 72 ячейки кэша

/// TTL в миллисекундах, время через которое кэш становится невалидным
const TTL_MILLIS: u64 = 3000;

const PLI_INTERVAL_MS: u64 = 500;

const MAX_VIDEO_BITRATE_BPS: u32 = 1_000_000;
const MAX_AUDIO_BITRATE_BPS: u32 = 64_000;

const REMB_INTERVAL: Duration = Duration::from_secs(1);

fn get_max_bitrate(mime_type: &str) -> u32 {
    match mime_type {
        MIME_TYPE_OPUS => MAX_AUDIO_BITRATE_BPS,
        MIME_TYPE_H264 | MIME_TYPE_VP8 => MAX_VIDEO_BITRATE_BPS,
        x => panic!("Неподдерживаемый тип кодека! {}", x),
    }
}

impl TrackLocalStaticRTP {
    /// returns a TrackLocalStaticRTP without rid.
    pub fn new(codec: RTCRtpCodecCapability, id: String, stream_id: String) -> Self {
        let max_bitrate_bps = get_max_bitrate(&codec.mime_type);
        let start_remb_time = Instant::now();
        // Начинаем с "10 секунд назад", чтобы можно было отправить сразу
        let initial_offset = Duration::from_secs(10).as_micros() as u64;
        TrackLocalStaticRTP {
            codec,
            bindings: DashMap::with_capacity(10),
            id,
            rid: None,
            stream_id,

            state: Mutex::new(TrackState::new()),
            rtp_cache: Arc::new(PCacheBuffer::new(
                Duration::from_millis(TTL_MILLIS),
                CAPACITY,
            )),

            pli_last_ms: AtomicU64::new(0),
            pli_interval_ms: PLI_INTERVAL_MS,

            bitrate: Mutex::new(BitrateState::new()),
            loss_stats: Mutex::new(ReceiverLossStats::new()),
            max_bitrate_bps,
            current_target_bitrate_bps: AtomicU32::new(max_bitrate_bps / 2),

            last_remb_sent: AtomicU64::new(initial_offset),
            start_remb_time,
            min_remb_interval: REMB_INTERVAL,
        }
    }

    /// returns a TrackLocalStaticRTP with rid.
    pub fn new_with_rid(
        codec: RTCRtpCodecCapability,
        id: String,
        rid: String,
        stream_id: String,
    ) -> Self {
        let max_bitrate_bps = get_max_bitrate(&codec.mime_type);
        let start_remb_time = Instant::now();
        // Начинаем с "10 секунд назад", чтобы можно было отправить сразу
        let initial_offset = Duration::from_secs(10).as_micros() as u64;
        TrackLocalStaticRTP {
            codec,
            bindings: DashMap::with_capacity(10),
            id,
            rid: Some(rid),
            stream_id,

            state: Mutex::new(TrackState::new()),
            rtp_cache: Arc::new(PCacheBuffer::new(
                Duration::from_millis(TTL_MILLIS),
                CAPACITY,
            )),

            pli_last_ms: AtomicU64::new(0),
            pli_interval_ms: PLI_INTERVAL_MS,

            bitrate: Mutex::new(BitrateState::new()),
            loss_stats: Mutex::new(ReceiverLossStats::new()),
            max_bitrate_bps,
            current_target_bitrate_bps: AtomicU32::new(max_bitrate_bps / 2),

            last_remb_sent: AtomicU64::new(initial_offset),
            start_remb_time,
            min_remb_interval: REMB_INTERVAL,
        }
    }

    /// codec gets the Codec of the track
    pub fn codec(&self) -> RTCRtpCodecCapability {
        self.codec.clone()
    }

    pub async fn any_binding_paused(&self) -> bool {
        self.bindings.iter().any(|entry| {
            let binding = entry.value();
            binding.sender_paused.load(Ordering::Relaxed)
        })
    }

    pub async fn all_binding_paused(&self) -> bool {
        self.bindings.iter().all(|entry| {
            let binding = entry.value();
            binding.sender_paused.load(Ordering::Relaxed)
        })
    }

    pub fn all_binding_paused_sync(&self) -> bool {
        self.bindings.iter().all(|entry| {
            let binding = entry.value();
            binding.sender_paused.load(Ordering::Relaxed)
        })
    }

    pub fn is_binding_active(&self, binding_ssrc: u32) -> bool {
        match self.bindings.get(&binding_ssrc) {
            // 2. Если элемент найден...
            Some(binding_ref) => !binding_ref.value().is_sender_paused(),
            // 4. Если элемент не найден, возвращаем false.
            None => false,
        }
    }

    /// seq - Последовательный номер в терминах получателей
    pub async fn cache_get(&self, seq: u16) -> Option<Arc<PCache>> {
        // В кэше хранятся данные без изменений
        self.rtp_cache.get_arc({
            let st = self.state.lock().await;
            // трансформируем в последовательный номер в терминах отправителя
            st.origin_seq(seq)
        })
    }

    /// Выполняется, когда мы изменяем источник данных для трека
    pub async fn replace_remote(self: Arc<Self>, remote_track: Arc<TrackRemote>) {
        // 1. Приводим исходящее смещение к начальному состоянию,
        // чтоб определить его заново в момент первого пришедшего пакета
        {
            let mut s = self.state.lock().await;
            s.out_offset = None;
        }

        // 2. Запись из mpsc канала в local_track
        // здесь должен быть минимальный буфер,
        // т.к. лучше потом отправить из кеша, чем пытаться отправить застрявший пакет из очереди
        let (rtp_sender, mut rtp_rx) = mpsc::channel::<Arc<PCache>>(64);
        let local_track = Arc::downgrade(&self);
        let rtp_writer = tokio::spawn(async move {
            while let Some(p_cache) = rtp_rx.recv().await {
                if let Some(local_track) = local_track.upgrade() {
                    if let Err(_err) = local_track.write_rtp(&p_cache.rtp).await {
                        // eprintln!("Ошибка записи данных в исходящий трек: {:?}", _err);
                    }
                } else {
                    break;
                }
            }
            // println!("Запись данных в трек остановлена!");
        });

        // 3. Чтение из remote_track в mpsc канал
        while let Ok(rtp) = remote_track.read_rtp_raw().await {
            // 1. Сохраняем в кэш оригинальный rtp без смещений! Так быстрее происходит сохранение в кэш
            // При восстановлении кеша нужно вернуть порядковый номер к оригинальному, чтоб найти его
            let p_cache = Arc::new(PCache {
                rtp,
                first_sent_at: Instant::now(),
            });
            self.rtp_cache.put(Arc::clone(&p_cache));

            // 2. Пытаемся отправить, если переполнен буфер, не ждём и позже в ответ на NACK берём из кэша
            // Без ожиданий, чтоб не замедлять процесс получения пакетов
            match rtp_sender.try_send(Arc::clone(&p_cache)) {
                Err(TrySendError::Closed(_)) => {
                    break;
                }
                Err(TrySendError::Full(_)) => {
                    // eprintln!("Ошибка отправки RTP данных: Буфер переполнен");
                }
                _ => {}
            }
        }

        // 4. Если remote_track перестал слать пакеты, то перестаём и записывать их
        rtp_writer.abort();
    }

    /// Получаем ssrc всех RTCPeerConnection подключений к этому треку
    pub fn bindings_ssrc(&self) -> Vec<u32> {
        self.bindings.iter().map(|b| b.key().clone()).collect()
    }

    pub fn bindings_ids(&self) -> Vec<String> {
        self.bindings.iter().map(|b| b.value().id.clone()).collect()
    }

    pub async fn write_rtp_with_extensions_to(
        &self,
        p: &rtp::packet::Packet,
        pkt_attrs: &PkgAttrs,
        extensions: &[rtp::extension::HeaderExtension],
        binding_ssrc: u32,
    ) -> Result<usize> {
        if let Some(b) = self.bindings.get(&binding_ssrc).map(|b| b.value().clone()) {
            // Prepare the extensions data
            let mut extension_error = None;
            let extension_data: HashMap<_, _> = extensions
                .iter()
                .flat_map(|extension| {
                    let buf = {
                        let mut buf = BytesMut::with_capacity(extension.marshal_size());
                        buf.resize(extension.marshal_size(), 0);
                        if let Err(err) = extension.marshal_to(&mut buf) {
                            extension_error = Some(Error::Util(err));
                            return None;
                        }

                        buf.freeze()
                    };

                    Some((extension.uri(), buf))
                })
                .collect();
            if let Some(err) = extension_error {
                return Err(err);
            }

            self.write_rtp_with_extensions_to_binding(p, pkt_attrs, &extension_data, b)
                .await
        } else {
            // Must return Ok(usize) to be consistent with write_rtp_with_extensions_attributes
            Err(Error::LocalTrackBindingNotFound)
        }
    }

    pub async fn write_rtp_with_extensions(
        &self,
        pkt: &rtp::packet::Packet,
        extensions: &[rtp::extension::HeaderExtension],
    ) -> Result<usize> {
        if self.all_binding_paused_sync() {
            // Если никто пакеты не получил, то меняем смещение так, чтоб не было пропущенных пакетов
            let mut st = self.state.lock().await;
            st.shift_offset(pkt.header.sequence_number, pkt.header.timestamp);
            return Ok(0);
        }
        let pkg_attrs = {
            let mut st = self.state.lock().await;
            st.get_pkg_attrs_set_last_out(
                self.kind(),
                pkt.header.sequence_number,
                pkt.header.timestamp,
            )
        };

        let mut n = 0;
        let mut write_errs = vec![];

        let bindings: Vec<Arc<TrackBinding>> =
            self.bindings.iter().map(|b| b.value().clone()).collect();
        // Prepare the extensions data
        let extension_data: HashMap<_, _> = extensions
            .iter()
            .flat_map(|extension| {
                let buf = {
                    let mut buf = BytesMut::with_capacity(extension.marshal_size());
                    buf.resize(extension.marshal_size(), 0);
                    if let Err(err) = extension.marshal_to(&mut buf) {
                        write_errs.push(Error::Util(err));
                        return None;
                    }

                    buf.freeze()
                };

                Some((extension.uri(), buf))
            })
            .collect();

        for b in bindings.into_iter() {
            match self
                .write_rtp_with_extensions_to_binding(&pkt, &pkg_attrs, &extension_data, b)
                .await
            {
                Ok(one_or_zero) => {
                    n += one_or_zero;
                }
                Err(err) => {
                    write_errs.push(err);
                }
            }
        }

        flatten_errs(write_errs)?;
        Ok(n)
    }

    pub async fn write_rtp_to(
        &self,
        pkt: &rtp::packet::Packet,
        binding_ssrc: u32,
    ) -> Result<usize> {
        let pkg_attrs = {
            let mut st = self.state.lock().await;
            st.get_pkg_attrs(
                self.kind(),
                pkt.header.sequence_number,
                pkt.header.timestamp,
            )
        };

        self.write_rtp_with_extensions_to(&pkt, &pkg_attrs, &[], binding_ssrc)
            .await
    }

    pub async fn set_muted(&self, muted: bool) {
        let bindings: Vec<Arc<TrackBinding>> =
            self.bindings.iter().map(|b| b.value().clone()).collect();
        bindings.iter().for_each(|b| {
            b.set_sender_paused(muted);
        });
    }

    pub async fn set_muted_for(&self, bindings_ssrc: Vec<(u32, bool)>) {
        let bindings: Vec<Arc<TrackBinding>> =
            self.bindings.iter().map(|b| b.value().clone()).collect();
        bindings.iter().for_each(|b| {
            if let Some((_, muted)) = bindings_ssrc.iter().find(|(ssrc, _)| *ssrc == b.ssrc) {
                b.set_sender_paused(*muted);
            }
        });
    }

    pub fn should_fire_pli(&self, now_ms: u64) -> bool {
        loop {
            let prev = self.pli_last_ms.load(Ordering::Relaxed);
            if now_ms.saturating_sub(prev) < self.pli_interval_ms {
                return false;
            }
            if self
                .pli_last_ms
                .compare_exchange_weak(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
            // кто-то другой успел обновить last_ms — пробуем снова

            // Мы проиграли гонку. Вместо того чтобы сразу бросаться в новую итерацию,
            // дадим процессору подсказку.
            std::hint::spin_loop();
        }
    }

    async fn write_rtp_with_extensions_to_binding(
        &self,
        p: &rtp::packet::Packet,
        pkg_attrs: &PkgAttrs,
        extension_data: &HashMap<Cow<'static, str>, Bytes>,
        binidng: Arc<TrackBinding>,
    ) -> Result<usize> {
        if binidng.is_sender_paused() {
            return Ok(0);
        }

        let mut pkt = p.clone();
        pkt.header.sequence_number = pkg_attrs.sequence_number;
        pkt.header.timestamp = pkg_attrs.timestamp;
        pkt.header.ssrc = binidng.ssrc;
        pkt.header.payload_type = binidng.payload_type;

        for ext in binidng.hdr_ext_ids.iter() {
            let payload = ext.payload.to_owned();
            if let Err(err) = pkt.header.set_extension(ext.id, payload) {
                return Err(Error::Rtp(err));
            }
        }

        for (uri, data) in extension_data.iter() {
            if let Some(id) = binidng
                .params
                .header_extensions
                .iter()
                .find(|ext| &ext.uri == uri)
                .map(|ext| ext.id)
            {
                if let Err(err) = pkt.header.set_extension(id as u8, data.clone()) {
                    return Err(Error::Rtp(err));
                }
            }
        }

        binidng.write_stream.write_rtp(&pkt).await
    }

    pub async fn handle_receiver_report(&self, receiver_ssrc: u32, report: &ReceptionReport) {
        let mut loss_stats_guard = self.loss_stats.lock().await;
        loss_stats_guard.update_with_rr(
            receiver_ssrc,
            report.fraction_lost,
            report.total_lost as i32,
        );

        // log::warn!(
        //     "\nRR от получателя {} для SSRC {}: потери {}%",
        //     receiver_ssrc,
        //     report.ssrc,
        //     report.fraction_lost
        // );
    }

    pub fn should_send_remb(&self) -> bool {
        let now_micros = self.instant_to_micros(Instant::now());
        let last_micros = self.last_remb_sent.load(Ordering::Acquire);

        if now_micros >= last_micros + self.min_remb_interval.as_micros() as u64 {
            // CAS для thread safety
            self.last_remb_sent
                .compare_exchange(
                    last_micros,
                    now_micros,
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
        } else {
            false
        }
    }

    fn instant_to_micros(&self, instant: Instant) -> u64 {
        instant.duration_since(self.start_remb_time).as_micros() as u64
    }

    // Используется для отправки REMB отчётов паблишеру трека
    pub fn adapt_bitrate(&self) -> Option<u32> {
        if !self.should_send_remb() {
            return None;
        }

        let current_target = self.current_target_bitrate_bps.load(Ordering::Relaxed);
        // Применяем изменение только если оно существенное (>5% изменения)
        let change_threshold = current_target / 20;
        // Выбираем минимально возможный размер битрейта (он же, шаг от изменения)
        let min_bitrate = self.max_bitrate_bps / 20;

        let real_bitrate = if let Ok(bitrate_guard) = self.bitrate.try_lock() {
            bitrate_guard.average_bitrate()
        } else {
            return None;
        };

        // Защита от некорректных значений битрейта
        if real_bitrate == 0 {
            return None;
        }

        // Если реальный битрейт скакнул, то считать реальным битрейтом целевой битрейт,
        // т.к. скачёк, скорее всего, случайный. Ведь не должно быть битрейта больше целевого
        let real_bitrate = std::cmp::min(real_bitrate, current_target);

        let should_reduce = if let Ok(loss_stats_guard) = self.loss_stats.try_lock() {
            loss_stats_guard.should_reduce()
        } else {
            return None;
        };

        match should_reduce {
            ReduceIncrease::Reduce(degradate) => {
                // Уменьшаем на основе реального битрейта, но не ниже минимального порога
                let new_bitrate =
                    std::cmp::max(real_bitrate * (100 - degradate) / 100, min_bitrate);

                if new_bitrate < current_target
                    && new_bitrate.abs_diff(current_target) > change_threshold
                {
                    log::warn!(
                        "\nУменьшение битрейта: target={}bps -> {}bps, real={}bps",
                        current_target,
                        new_bitrate,
                        real_bitrate,
                    );
                    // Сохраняем последнее время отправки
                    self.last_remb_sent
                        .store(self.instant_to_micros(Instant::now()), Ordering::SeqCst);
                    self.current_target_bitrate_bps
                        .store(new_bitrate, Ordering::SeqCst);
                    Some(new_bitrate)
                } else {
                    None
                }
            }
            ReduceIncrease::Increase => {
                // Увеличиваем осторожно, проверяя что реальный битрейт может поддерживаться
                let proposed_increase = real_bitrate + min_bitrate;

                // Не увеличиваем больше чем на 50% от реального битрейта за раз
                // Битрейт не может быть больше максимального
                let max_safe_increase = real_bitrate + (real_bitrate / 2);
                let new_bitrate = std::cmp::min(proposed_increase, max_safe_increase);
                let new_bitrate = std::cmp::min(new_bitrate, self.max_bitrate_bps);

                if new_bitrate > current_target
                    && new_bitrate.abs_diff(current_target) > change_threshold
                {
                    log::warn!(
                        "\nУвеличение битрейта: target={}bps -> {}bps, real={}bps",
                        current_target,
                        new_bitrate,
                        real_bitrate,
                    );
                    // Сохраняем последнее время отправки
                    self.last_remb_sent
                        .store(self.instant_to_micros(Instant::now()), Ordering::SeqCst);
                    self.current_target_bitrate_bps
                        .store(new_bitrate, Ordering::SeqCst);
                    Some(new_bitrate)
                } else {
                    None
                }
            }
            ReduceIncrease::NoChange => None,
        }
    }
}

#[async_trait]
impl TrackLocal for TrackLocalStaticRTP {
    /// bind is called by the PeerConnection after negotiation is complete
    /// This asserts that the code requested is supported by the remote peer.
    /// If so it setups all the state (SSRC and PayloadType) to have a call
    async fn bind(&self, t: &TrackLocalContext) -> Result<RTCRtpCodecParameters> {
        let parameters = RTCRtpCodecParameters {
            capability: self.codec.clone(),
            ..Default::default()
        };
        let mut hdr_ext_ids = vec![];
        if let Some(id) = t
            .header_extensions()
            .iter()
            .find(|e| e.uri == ::sdp::extmap::SDES_MID_URI)
            .map(|e| e.id as u8)
        {
            if let Some(payload) = t
                .mid
                .as_ref()
                .map(|mid| Bytes::copy_from_slice(mid.as_bytes()))
            {
                hdr_ext_ids.push(rtp::header::Extension { id, payload });
            }
        }

        if let Some(id) = t
            .header_extensions()
            .iter()
            .find(|e| e.uri == ::sdp::extmap::SDES_RTP_STREAM_ID_URI)
            .map(|e| e.id as u8)
        {
            if let Some(payload) = self.rid().map(|rid| rid.to_owned().into()) {
                hdr_ext_ids.push(rtp::header::Extension { id, payload });
            }
        }

        let (codec, match_type) = codec_parameters_fuzzy_search(&parameters, t.codec_parameters());
        if match_type != CodecMatch::None {
            {
                self.bindings.insert(
                    t.ssrc(),
                    Arc::new(TrackBinding {
                        id: t.id(),
                        ssrc: t.ssrc(),
                        payload_type: codec.payload_type,
                        params: t.params.clone(),
                        write_stream: t.write_stream(),
                        sender_paused: t.paused.clone(),
                        hdr_ext_ids,
                    }),
                );
            }

            Ok(codec)
        } else {
            Err(Error::ErrUnsupportedCodec)
        }
    }

    /// unbind implements the teardown logic when the track is no longer needed. This happens
    /// because a track has been stopped.
    async fn unbind(&self, t: &TrackLocalContext) -> Result<()> {
        self.bindings.remove(&t.ssrc());

        Ok(())
    }

    /// id is the unique identifier for this Track. This should be unique for the
    /// stream, but doesn't have to globally unique. A common example would be 'audio' or 'video'
    /// and StreamID would be 'desktop' or 'webcam'
    fn id(&self) -> &str {
        self.id.as_str()
    }

    /// RID is the RTP Stream ID for this track.
    fn rid(&self) -> Option<&str> {
        self.rid.as_deref()
    }

    /// stream_id is the group this track belongs too. This must be unique
    fn stream_id(&self) -> &str {
        self.stream_id.as_str()
    }

    /// kind controls if this TrackLocal is audio or video
    fn kind(&self) -> RTPCodecType {
        if self.codec.mime_type.starts_with("audio/") {
            RTPCodecType::Audio
        } else if self.codec.mime_type.starts_with("video/") {
            RTPCodecType::Video
        } else {
            RTPCodecType::Unspecified
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[async_trait]
impl TrackLocalWriter for TrackLocalStaticRTP {
    /// `write_rtp` writes a RTP Packet to the TrackLocalStaticRTP
    /// If one PeerConnection fails the packets will still be sent to
    /// all PeerConnections. The error message will contain the ID of the failed
    /// PeerConnections so you can remove them
    ///
    /// If the RTCRtpSender direction is such that no packets should be sent, any call to this
    /// function are blocked internally. Care must be taken to not increase the sequence number
    /// while the sender is paused. While the actual _sending_ is blocked, the receiver will
    /// miss out when the sequence number "rolls over", which in turn will break SRTP.
    async fn write_rtp(&self, pkt: &rtp::packet::Packet) -> Result<usize> {
        self.write_rtp_with_extensions(pkt, &[]).await
    }

    /// write writes a RTP Packet as a buffer to the TrackLocalStaticRTP
    /// If one PeerConnection fails the packets will still be sent to
    /// all PeerConnections. The error message will contain the ID of the failed
    /// PeerConnections so you can remove them
    async fn write(&self, mut b: &[u8]) -> Result<usize> {
        let pkt = rtp::packet::Packet::unmarshal(&mut b)?;
        self.write_rtp(&pkt).await?;
        Ok(b.len())
    }
}
