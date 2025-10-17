use crate::rtp_transceiver::rtp_codec::RTPCodecType;

#[derive(Clone, Debug)]
pub struct TrackState {
    last_out_seq: u16, // переживает все переключения источников
    last_out_ts: u32,  // переживает все переключения источников
    started_at_ts: i64,
    marker: bool, // marker = true означает, что это последний пакет видеофрейма.
    // Это сигнал для джиттер-буфера и декодера, что можно собрать все полученные пакеты этого фрейма и отправить их на декодирование.
    // Используется после паузы
    pub out_offset: Option<(
        u16, /* смещение порядкового номера */
        u32, /* смещение временной метки timestamp */
    )>,
}

pub struct PkgAttrs {
    pub sequence_number: u16,
    pub timestamp: u32,
    pub marker: bool,
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
