use std::time::Instant;

use bytes::Bytes;
use media::Sample;
use rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use tokio::sync::mpsc;
use tokio::time::Duration;
use waitgroup::WaitGroup;

use super::*;
use crate::api::media_engine::{MIME_TYPE_OPUS, MIME_TYPE_VP8};
use crate::error::Result;
use crate::peer_connection::peer_connection_state::RTCPeerConnectionState;
use crate::peer_connection::peer_connection_test::{
    close_pair_now, create_vnet_pair, signal_pair, until_connection_state,
};
use crate::rtp_transceiver::rtp_codec::RTCRtpHeaderExtensionParameters;
use crate::rtp_transceiver::{RTCPFeedback, RTCRtpCodecCapability};
use crate::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use crate::track::track_local::TrackLocal;

lazy_static! {
    static ref P: RTCRtpParameters = RTCRtpParameters {
        codecs: vec![RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_string(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
                rtcp_feedback: vec![RTCPFeedback {
                    typ: "nack".to_owned(),
                    parameter: "".to_owned(),
                }],
            },
            payload_type: 111,
            ..Default::default()
        }],
        header_extensions: vec![
            RTCRtpHeaderExtensionParameters {
                uri: "urn:ietf:params:rtp-hdrext:sdes:mid".to_owned(),
                ..Default::default()
            },
            RTCRtpHeaderExtensionParameters {
                uri: "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id".to_owned(),
                ..Default::default()
            },
            RTCRtpHeaderExtensionParameters {
                uri: "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id".to_owned(),
                ..Default::default()
            },
        ],
    };
}

//use log::LevelFilter;
//use std::io::Write;

#[tokio::test]
async fn test_set_rtp_parameters() -> Result<()> {
    /*env_logger::Builder::new()
    .format(|buf, record| {
        writeln!(
            buf,
            "{}:{} [{}] {} - {}",
            record.file().unwrap_or("unknown"),
            record.line().unwrap_or(0),
            record.level(),
            chrono::Local::now().format("%H:%M:%S.%6f"),
            record.args()
        )
    })
    .filter(None, LevelFilter::Trace)
    .init();*/

    let (mut sender, mut receiver, wan) = create_vnet_pair().await?;

    let outgoing_track: Arc<dyn TrackLocal + Send + Sync> = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_VP8.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    sender.add_track(Arc::clone(&outgoing_track)).await?;

    // Those parameters wouldn't make sense in a real application,
    // but for the sake of the test we just need different values.

    let (seen_packet_tx, mut seen_packet_rx) = mpsc::channel::<()>(1);
    let seen_packet_tx = Arc::new(Mutex::new(Some(seen_packet_tx)));
    receiver.on_track(Box::new(move |_, receiver, _| {
        let seen_packet_tx2 = Arc::clone(&seen_packet_tx);
        Box::pin(async move {
            receiver.set_rtp_parameters(P.clone()).await;

            let tracks = receiver.tracks().await;
            assert_eq!(tracks.len(), 1);
            let t = tracks.first().unwrap();

            let incoming_track_codecs = t.codec();

            assert_eq!(P.header_extensions, t.params().header_extensions);
            assert_eq!(
                P.codecs[0].capability.mime_type,
                incoming_track_codecs.capability.mime_type
            );
            assert_eq!(
                P.codecs[0].capability.clock_rate,
                incoming_track_codecs.capability.clock_rate
            );
            assert_eq!(
                P.codecs[0].capability.channels,
                incoming_track_codecs.capability.channels
            );
            assert_eq!(
                P.codecs[0].capability.sdp_fmtp_line,
                incoming_track_codecs.capability.sdp_fmtp_line
            );
            assert_eq!(
                P.codecs[0].capability.rtcp_feedback,
                incoming_track_codecs.capability.rtcp_feedback
            );
            assert_eq!(P.codecs[0].payload_type, incoming_track_codecs.payload_type);

            {
                let mut done = seen_packet_tx2.lock().await;
                done.take();
            }
        })
    }));

    let wg = WaitGroup::new();

    until_connection_state(&mut sender, &wg, RTCPeerConnectionState::Connected).await;
    until_connection_state(&mut receiver, &wg, RTCPeerConnectionState::Connected).await;

    signal_pair(&mut sender, &mut receiver).await?;

    wg.wait().await;

    if let Some(v) = outgoing_track
        .as_any()
        .downcast_ref::<TrackLocalStaticSample>()
    {
        v.write_sample(&Sample {
            data: Bytes::from_static(&[0xAA]),
            duration: Duration::from_secs(1),
            ..Default::default()
        })
        .await?;
    } else {
        panic!();
    }

    let _ = seen_packet_rx.recv().await;
    {
        let mut w = wan.lock().await;
        w.stop().await?;
    }
    close_pair_now(&sender, &receiver).await;

    Ok(())
}

// Assert that SetReadDeadline works as expected
// This test uses VNet since we must have zero loss
#[tokio::test]
async fn test_rtp_receiver_set_read_deadline() -> Result<()> {
    let (mut sender, mut receiver, wan) = create_vnet_pair().await?;

    let track: Arc<dyn TrackLocal + Send + Sync> = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_VP8.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    sender.add_track(Arc::clone(&track)).await?;

    let (seen_packet_tx, mut seen_packet_rx) = mpsc::channel::<()>(1);
    let seen_packet_tx = Arc::new(Mutex::new(Some(seen_packet_tx)));
    receiver.on_track(Box::new(move |track, receiver, _| {
        let seen_packet_tx2 = Arc::clone(&seen_packet_tx);
        Box::pin(async move {
            // First call will not error because we cache for probing
            let result = tokio::time::timeout(Duration::from_secs(1), track.read_rtp()).await;
            assert!(
                result.is_ok(),
                " First call will not error because we cache for probing"
            );

            let result = tokio::time::timeout(Duration::from_secs(1), track.read_rtp()).await;
            assert!(result.is_err());

            let result = tokio::time::timeout(Duration::from_secs(1), receiver.read_rtcp()).await;
            assert!(result.is_err());

            {
                let mut done = seen_packet_tx2.lock().await;
                done.take();
            }
        })
    }));

    let wg = WaitGroup::new();
    until_connection_state(&mut sender, &wg, RTCPeerConnectionState::Connected).await;
    until_connection_state(&mut receiver, &wg, RTCPeerConnectionState::Connected).await;

    signal_pair(&mut sender, &mut receiver).await?;

    wg.wait().await;

    if let Some(v) = track.as_any().downcast_ref::<TrackLocalStaticSample>() {
        v.write_sample(&Sample {
            data: Bytes::from_static(&[0xAA]),
            duration: Duration::from_secs(1),
            ..Default::default()
        })
        .await?;
    } else {
        panic!();
    }

    let _ = seen_packet_rx.recv().await;
    {
        let mut w = wan.lock().await;
        w.stop().await?;
    }
    close_pair_now(&sender, &receiver).await;

    Ok(())
}

// This test uses VNet since we must have zero loss
#[tokio::test]
async fn test_rtp_receiver_internal_read_single_rtcp_latency() -> Result<()> {
    // 1) Пара peer'ов на виртуальной сети
    let (mut sender_pc, mut receiver_pc, wan) = create_vnet_pair().await?;

    // 2) Локальный трек как в примере
    let track: Arc<dyn TrackLocal + Send + Sync> = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_VP8.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));
    sender_pc.add_track(Arc::clone(&track)).await?;

    // 3) Канал для получения RTCRtpReceiver из on_track
    let (rx_tx, mut rx_rx) = mpsc::channel(1);
    receiver_pc.on_track(Box::new(move |_remote_track, rtp_receiver, _| {
        let rx_tx = rx_tx.clone();
        Box::pin(async move {
            // Отдадим наружу хэндл ресивера
            let _ = rx_tx.send(rtp_receiver).await;
        })
    }));

    // 4) Сведение
    let wg = WaitGroup::new();
    until_connection_state(&mut sender_pc, &wg, RTCPeerConnectionState::Connected).await;
    until_connection_state(&mut receiver_pc, &wg, RTCPeerConnectionState::Connected).await;
    signal_pair(&mut sender_pc, &mut receiver_pc).await?;
    wg.wait().await;

    // 5) Отправим ОДИН RTP-семпл, чтобы триггернуть on_track на приёмнике
    if let Some(v) = track.as_any().downcast_ref::<TrackLocalStaticSample>() {
        v.write_sample(&Sample {
            data: Bytes::from_static(&[0x90, 0x90, 0x90]), // произвольный крошечный payload
            duration: Duration::from_millis(33),
            ..Default::default()
        })
        .await?;
    } else {
        panic!("unexpected track type");
    }

    // 6) Теперь можно корректно дождаться RTCRtpReceiver из on_track
    let rtp_receiver = tokio::time::timeout(Duration::from_secs(2), rx_rx.recv())
        .await
        .expect("on_track did not fire in time") // timeout
        .expect("on_track channel closed"); // channel closed

    let mut ssrc = None;

    // На PC могут быть несколько sender'ов — найдём тот, который несёт наш track
    for s in sender_pc.get_senders().await {
        // track() вернёт Some для медиасендеров
        if let Some(t) = s.track().await {
            // Сопоставим по id, чтобы быть уверенными, что это именно наш TrackLocalStaticSample
            if t.id() == track.id() {
                let params = s.get_parameters().await;
                if let Some(enc) = params.encodings.get(0) {
                    ssrc = Some(enc.ssrc);
                }
                break;
            }
        }
    }
    let ssrc = ssrc.expect("failed to resolve media SSRC for the track");

    // 7) Запустим цикл чтения RTCP: после каждого возврата шлём отметку времени
    use tokio::time::{Duration, Instant};
    const N: usize = 10000;

    let (done_tx, mut done_rx) = mpsc::channel::<Instant>(N);
    let reader = rtp_receiver.clone();
    tokio::spawn(async move {
        for _ in 0..N {
            let _ = reader.read_rtcp().await; // внутри дойдёт до RTPReceiverInternal::read(...)
            let _ = done_tx.send(Instant::now()).await;
        }
    });

    // Небольшой yield, чтобы reader гарантированно вошёл в select! и ждал
    // tokio::time::sleep(Duration::from_millis(20)).await;

    // 8) Сформируем одиночный PLI и отправим его с отправителя
    let mut latencies = Vec::with_capacity(N);
    for _ in 0..N {
        let pli = PictureLossIndication {
            sender_ssrc: ssrc,
            media_ssrc: ssrc, // критично: совпадает с media SSRC трека
        };
        let pkts: Vec<Box<dyn rtcp::packet::Packet + Send + Sync>> = vec![Box::new(pli)];

        // Засекаем момент перед отправкой конкретного пакета
        let t0 = Instant::now();
        sender_pc.write_rtcp(&pkts).await?;

        // 9) Дождёмся окончания чтения для ЭТОГО пакета и запишем задержку
        let t_read = tokio::time::timeout(Duration::from_secs(2), done_rx.recv())
            .await
            .expect("read_rtcp() timed out")
            .expect("done channel closed");

        latencies.push(t_read.duration_since(t0));
    }

    let total_ns: u128 = latencies.iter().map(|d| d.as_nanos()).sum();
    let avg_ns = total_ns / (latencies.len() as u128);

    let max_ns = latencies.iter().map(|d| d.as_nanos()).max().unwrap_or(0);
    let min_ns = latencies.iter().map(|d| d.as_nanos()).min().unwrap_or(0);

    eprintln!(
        "[read RTCP ×{N}] avg = {:.3} µs, min = {:.3} µs, max = {:.3} µs",
        (avg_ns as f64) / 1_000.0,
        (min_ns as f64) / 1_000.0,
        (max_ns as f64) / 1_000.0
    );

    // SLA по средней задержке — подстрой под ваш CI
    let sla_avg = Duration::from_micros(50);
    assert!(
        Duration::from_nanos(avg_ns as u64) <= sla_avg,
        "Average RTCP read latency {:.1} µs exceeded SLA {:?}",
        (avg_ns as f64) / 1_000.0,
        sla_avg
    );

    // 11) Корректное завершение
    {
        let mut w = wan.lock().await;
        w.stop().await?;
    }
    close_pair_now(&sender_pc, &receiver_pc).await;

    Ok(())
}

const TAG: &[u8; 8] = b"WRSSEQ01";

// Ищем TAG в payload и читаем u32 (BE) после него
fn extract_seq_from_payload(payload: &[u8]) -> Option<u32> {
    payload
        .windows(TAG.len())
        .position(|w| w == TAG)
        .and_then(|pos| {
            let start = pos + TAG.len();
            if payload.len() >= start + 4 {
                Some(u32::from_be_bytes([
                    payload[start],
                    payload[start + 1],
                    payload[start + 2],
                    payload[start + 3],
                ]))
            } else {
                None
            }
        })
}

#[tokio::test]
async fn test_rtp_receiver_internal_read_single_rtp_latency() -> Result<()> {
    // 1) Пара peer'ов на виртуальной сети
    let (mut sender_pc, mut receiver_pc, wan) = create_vnet_pair().await?;

    // 2) Локальный трек (как в твоём примере)
    let track: Arc<dyn TrackLocal + Send + Sync> = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_VP8.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));
    sender_pc.add_track(Arc::clone(&track)).await?;

    // 3) Получим TrackRemote из on_track
    let (trk_tx, mut trk_rx) = mpsc::channel(1);
    receiver_pc.on_track(Box::new(move |remote_track, _rtp_receiver, _| {
        let trk_tx = trk_tx.clone();
        Box::pin(async move {
            let _ = trk_tx.send(remote_track).await;
        })
    }));

    // 4) Сведение
    let wg = WaitGroup::new();
    until_connection_state(&mut sender_pc, &wg, RTCPeerConnectionState::Connected).await;
    until_connection_state(&mut receiver_pc, &wg, RTCPeerConnectionState::Connected).await;
    signal_pair(&mut sender_pc, &mut receiver_pc).await?;
    wg.wait().await;

    // 5) Отправим один крошечный сэмпл, чтобы триггернуть on_track
    if let Some(v) = track.as_any().downcast_ref::<TrackLocalStaticSample>() {
        v.write_sample(&Sample {
            data: Bytes::from_static(&[0xAB]), // очень маленький payload -> один RTP-пакет
            duration: Duration::from_millis(20),
            ..Default::default()
        })
        .await?;
    } else {
        panic!("unexpected track type");
    }

    // 6) Дождёмся TrackRemote из on_track
    let remote_track = tokio::time::timeout(Duration::from_secs(2), trk_rx.recv())
        .await
        .expect("on_track did not fire in time")
        .expect("on_track channel closed");

    // 7) Воркер: ожидаем строго последовательные seq в payload и шлём Instant только для них

    const N: usize = 1000; // настрои под свою длительность теста
    let (done_tx, mut done_rx) = mpsc::channel::<(u32, Instant)>(N);

    let rt_for_reader = remote_track.clone();
    tokio::spawn(async move {
        let mut expected: u32 = 0;

        // Игнорируем любые не-тестовые пакеты (например, тот, что триггерил on_track)
        // Читаем, пока не увидим N "наших" пакетов по порядку
        while (expected as usize) < N {
            match rt_for_reader.read_rtp().await {
                Ok(pkt) => {
                    if let Some(seq) = extract_seq_from_payload(&pkt.payload) {
                        if seq == expected {
                            let _ = done_tx.send((seq, Instant::now())).await;
                            expected = expected.wrapping_add(1);
                        } else {
                            // чужой или «будущий» пакет — игнорируем
                        }
                    } else {
                        // не наш пакет — игнорируем
                    }
                }
                Err(_) => break,
            }
        }
    });

    // 8) Основной цикл: отправляем N семплов с TAG+seq и меряем t_read - t0
    let mut latencies = Vec::with_capacity(N);

    for i in 0..N {
        let seq = i as u32;

        // payload = TAG || seq_be || небольшой наполнитель
        let mut payload = Vec::with_capacity(TAG.len() + 4 + 4);
        payload.extend_from_slice(TAG);
        payload.extend_from_slice(&seq.to_be_bytes());
        payload.extend_from_slice(&[0x90, 0x90, 0x90, 0x90]); // filler; всё равно уместится в 1 RTP

        let sample = Sample {
            data: Bytes::from(payload),
            duration: Duration::from_millis(16),
            ..Default::default()
        };

        let t0 = Instant::now();
        if let Some(v) = track.as_any().downcast_ref::<TrackLocalStaticSample>() {
            v.write_sample(&sample).await?;
        } else {
            panic!("unexpected track type");
        }

        // Дождёмся квитанции именно за этот seq (воркер отправит их по порядку)
        let (seq_back, t_read) = tokio::time::timeout(Duration::from_secs(2), done_rx.recv())
            .await
            .expect("read_rtp() timed out")
            .expect("done channel closed");

        assert_eq!(
            seq_back, seq,
            "seq mismatch: sent {}, got {}",
            seq, seq_back
        );

        latencies.push(t_read.duration_since(t0));

        // Больше не нужен sleep — ассоциация send→recv теперь детерминированная
    }

    // 9) Статистика: avg / min / max (в мкс)
    let total_ns: u128 = latencies.iter().map(|d| d.as_nanos()).sum();
    let avg_ns = total_ns / (latencies.len() as u128);
    let max_ns = latencies.iter().map(|d| d.as_nanos()).max().unwrap_or(0);
    let min_ns = latencies.iter().map(|d| d.as_nanos()).min().unwrap_or(0);

    eprintln!(
        "[read RTP ×{N}] avg = {:.3} µs, min = {:.3} µs, max = {:.3} µs",
        (avg_ns as f64) / 1_000.0,
        (min_ns as f64) / 1_000.0,
        (max_ns as f64) / 1_000.0
    );

    // 10) SLA по средней задержке — подстрой под свой CI/окружение
    let sla_avg = Duration::from_micros(50); // пример: 2 ms
    assert!(
        Duration::from_nanos(avg_ns as u64) <= sla_avg,
        "Average RTP read latency {:.1} µs exceeded SLA {:?}",
        (avg_ns as f64) / 1_000.0,
        sla_avg
    );

    // 11) Корректное завершение (как было)
    {
        let mut w = wan.lock().await;
        w.stop().await?;
    }
    close_pair_now(&sender_pc, &receiver_pc).await;

    Ok(())
}
