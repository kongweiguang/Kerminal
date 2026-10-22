//! SFTP 写 ACK 记账；只有所有在途请求确认并完成 sync 后才推进可信断点。
//!
//! @author kongweiguang

use std::collections::VecDeque;

/// 一个已提交给 SFTP client 的写请求序号，ACK 必须按请求顺序消费。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteTicket(u64);

/// 流水线容量或服务器确认顺序不满足可信进度条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePipelineError {
    InvalidPolicy,
    EmptyWrite,
    PacketTooLarge,
    WindowFull,
    SequenceExhausted,
    UnexpectedAcknowledgement,
    AcknowledgementOutOfOrder,
    AcknowledgementsPending,
    NothingToSync,
    OffsetOverflow,
}

#[derive(Debug, Clone, Copy)]
struct PendingWrite {
    ticket: WriteTicket,
    bytes: u32,
}

/// 单传输流水线限制请求尺寸和未确认字节；检查点不推测服务端可能已经收到的内容。
#[derive(Debug)]
pub struct SftpWriteAckPipeline {
    confirmed_offset: u64,
    next_sequence: u64,
    packet_bytes: u32,
    window_bytes: u64,
    pending_bytes: u64,
    acknowledged_since_sync: u64,
    pending: VecDeque<PendingWrite>,
}

impl SftpWriteAckPipeline {
    /// 从经验证的配置和上次可信偏移创建新流水线，不根据 partial 文件长度自推检查点。
    pub fn new(
        confirmed_offset: u64,
        pipeline_depth: usize,
        packet_bytes: u32,
    ) -> Result<Self, WritePipelineError> {
        if pipeline_depth == 0 || packet_bytes == 0 {
            return Err(WritePipelineError::InvalidPolicy);
        }
        let window_bytes = (pipeline_depth as u64)
            .checked_mul(packet_bytes as u64)
            .ok_or(WritePipelineError::InvalidPolicy)?;
        Ok(Self {
            confirmed_offset,
            next_sequence: 1,
            packet_bytes,
            window_bytes,
            pending_bytes: 0,
            acknowledged_since_sync: 0,
            pending: VecDeque::new(),
        })
    }

    /// 保留一个有界写请求槽；caller 只有在实际写入对应 packet 后才等待该 ticket 的 ACK。
    pub fn reserve_write(&mut self, bytes: u32) -> Result<WriteTicket, WritePipelineError> {
        if bytes == 0 {
            return Err(WritePipelineError::EmptyWrite);
        }
        if bytes > self.packet_bytes {
            return Err(WritePipelineError::PacketTooLarge);
        }
        let in_flight = self
            .pending_bytes
            .checked_add(self.acknowledged_since_sync)
            .ok_or(WritePipelineError::WindowFull)?;
        if in_flight
            .checked_add(bytes as u64)
            .is_none_or(|next| next > self.window_bytes)
        {
            return Err(WritePipelineError::WindowFull);
        }
        let next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(WritePipelineError::SequenceExhausted)?;
        let ticket = WriteTicket(self.next_sequence);
        self.next_sequence = next_sequence;
        self.pending_bytes += bytes as u64;
        self.pending.push_back(PendingWrite { ticket, bytes });
        Ok(ticket)
    }

    /// 将一个 SFTP status ACK 与最早未确认写请求配对，拒绝重复或乱序回执。
    pub fn acknowledge(&mut self, ticket: WriteTicket) -> Result<(), WritePipelineError> {
        let Some(front) = self.pending.front().copied() else {
            return Err(WritePipelineError::UnexpectedAcknowledgement);
        };
        if front.ticket != ticket {
            return Err(WritePipelineError::AcknowledgementOutOfOrder);
        }
        self.pending.pop_front();
        self.pending_bytes -= front.bytes as u64;
        self.acknowledged_since_sync = self
            .acknowledged_since_sync
            .checked_add(front.bytes as u64)
            .ok_or(WritePipelineError::WindowFull)?;
        Ok(())
    }

    /// 在 SFTP flush 确认所有写请求且 fsync 成功后，原子推进检查点偏移。
    pub fn confirm_synced(&mut self) -> Result<u64, WritePipelineError> {
        if !self.pending.is_empty() {
            return Err(WritePipelineError::AcknowledgementsPending);
        }
        if self.acknowledged_since_sync == 0 {
            return Err(WritePipelineError::NothingToSync);
        }
        self.confirmed_offset = self
            .confirmed_offset
            .checked_add(self.acknowledged_since_sync)
            .ok_or(WritePipelineError::OffsetOverflow)?;
        self.acknowledged_since_sync = 0;
        Ok(self.confirmed_offset)
    }

    /// 返回 ACK 与 sync 均完成的偏移，只能用于发布 progress/checkpoint。
    pub const fn confirmed_offset(&self) -> u64 {
        self.confirmed_offset
    }

    /// 返回尚未 ACK 或尚未 sync 的字节总量，限制实际在途内存窗口。
    pub const fn in_flight_bytes(&self) -> u64 {
        self.pending_bytes + self.acknowledged_since_sync
    }

    /// 返回网络参数允许的最大在途字节窗口。
    pub const fn window_bytes(&self) -> u64 {
        self.window_bytes
    }
}
