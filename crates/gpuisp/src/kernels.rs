//! The compute pipelines, their descriptor layouts and pool, and the per-frame command
//! buffer, fence and timestamp queries: made once per ISP and kept.

use std::sync::Arc;
use std::time::Duration;

use ash::vk;

use crate::context::Inner;
use crate::error::{GpuError, VkContext};

/// SPIR-V of `shaders/*.comp` (`shaders/build.sh` makes them).
const FULL: &[u8] = include_bytes!("../shaders/full.spv");
const HALF: &[u8] = include_bytes!("../shaders/half.spv");
const STATS: &[u8] = include_bytes!("../shaders/stats.spv");

/// Descriptor sets the ISP may hold at once: the tables, inputs (staging, device copy and
/// imported dma-bufs) and outputs (staging, device copy and the export ring).
const MAX_SETS: u32 = 96;

pub(crate) struct Kernels {
    ctx: Arc<Inner>,
    /// Set 0: parameters, tone table, lens shading, statistics. Set 1: raw input. Set 2:
    /// output.
    pub set_layouts: [vk::DescriptorSetLayout; 3],
    pub layout: vk::PipelineLayout,
    pub full: vk::Pipeline,
    pub half: vk::Pipeline,
    pub stats: vk::Pipeline,
    pool: vk::DescriptorPool,
    cmd_pool: vk::CommandPool,
    pub cmd: vk::CommandBuffer,
    pub fence: vk::Fence,
    pub queries: Option<vk::QueryPool>,
}

impl Drop for Kernels {
    fn drop(&mut self) {
        let d = &self.ctx.device;
        // SAFETY: the owner waited on the fence; nothing is in flight.
        unsafe {
            if let Some(q) = self.queries {
                d.destroy_query_pool(q, None);
            }
            d.destroy_fence(self.fence, None);
            d.destroy_command_pool(self.cmd_pool, None);
            d.destroy_descriptor_pool(self.pool, None);
            for p in [self.full, self.half, self.stats] {
                d.destroy_pipeline(p, None);
            }
            d.destroy_pipeline_layout(self.layout, None);
            for l in self.set_layouts {
                d.destroy_descriptor_set_layout(l, None);
            }
        }
    }
}

fn spirv(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .collect()
}

impl Kernels {
    pub fn new(ctx: &Arc<Inner>) -> Result<Self, GpuError> {
        let d = &ctx.device;
        let binding = |b: u32| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let set0 = [binding(0), binding(1), binding(2), binding(3)];
        let set1 = [binding(0)];
        let mut k = Self {
            ctx: Arc::clone(ctx),
            set_layouts: [vk::DescriptorSetLayout::null(); 3],
            layout: vk::PipelineLayout::null(),
            full: vk::Pipeline::null(),
            half: vk::Pipeline::null(),
            stats: vk::Pipeline::null(),
            pool: vk::DescriptorPool::null(),
            cmd_pool: vk::CommandPool::null(),
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            queries: None,
        };
        for (i, b) in [&set0[..], &set1[..], &set1[..]].into_iter().enumerate() {
            let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(b);
            // SAFETY: valid create info; `k` destroys what is made (null handles are ignored).
            k.set_layouts[i] = unsafe { d.create_descriptor_set_layout(&info, None) }
                .ctx("vkCreateDescriptorSetLayout")?;
        }
        let info = vk::PipelineLayoutCreateInfo::default().set_layouts(&k.set_layouts);
        // SAFETY: as above.
        k.layout =
            unsafe { d.create_pipeline_layout(&info, None) }.ctx("vkCreatePipelineLayout")?;
        let mut modules = Vec::new();
        for code in [FULL, HALF, STATS] {
            let words = spirv(code);
            let info = vk::ShaderModuleCreateInfo::default().code(&words);
            // SAFETY: SPIR-V made by glslc for Vulkan 1.2.
            match unsafe { d.create_shader_module(&info, None) } {
                Ok(m) => modules.push(m),
                Err(e) => {
                    // SAFETY: modules made above, unused.
                    modules
                        .iter()
                        .for_each(|&m| unsafe { d.destroy_shader_module(m, None) });
                    return Err(GpuError::Vulkan {
                        call: "vkCreateShaderModule",
                        result: e,
                    });
                }
            }
        }
        let infos: Vec<vk::ComputePipelineCreateInfo> = modules
            .iter()
            .map(|&m| {
                vk::ComputePipelineCreateInfo::default()
                    .stage(
                        vk::PipelineShaderStageCreateInfo::default()
                            .stage(vk::ShaderStageFlags::COMPUTE)
                            .module(m)
                            .name(c"main"),
                    )
                    .layout(k.layout)
            })
            .collect();
        // SAFETY: valid create infos; the modules are not needed afterwards.
        let made = unsafe { d.create_compute_pipelines(vk::PipelineCache::null(), &infos, None) };
        // SAFETY: as above.
        modules
            .iter()
            .for_each(|&m| unsafe { d.destroy_shader_module(m, None) });
        let pipelines = made.map_err(|(_, e)| GpuError::Vulkan {
            call: "vkCreateComputePipelines",
            result: e,
        })?;
        [k.full, k.half, k.stats] = [pipelines[0], pipelines[1], pipelines[2]];
        let sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::STORAGE_BUFFER,
            descriptor_count: MAX_SETS * 4,
        }];
        let info = vk::DescriptorPoolCreateInfo::default()
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
            .max_sets(MAX_SETS)
            .pool_sizes(&sizes);
        // SAFETY: as above.
        k.pool = unsafe { d.create_descriptor_pool(&info, None) }.ctx("vkCreateDescriptorPool")?;
        let info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(ctx.queue_family);
        // SAFETY: as above.
        k.cmd_pool = unsafe { d.create_command_pool(&info, None) }.ctx("vkCreateCommandPool")?;
        let info = vk::CommandBufferAllocateInfo::default()
            .command_pool(k.cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: as above.
        k.cmd = unsafe { d.allocate_command_buffers(&info) }.ctx("vkAllocateCommandBuffers")?[0];
        // SAFETY: as above.
        k.fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None) }
            .ctx("vkCreateFence")?;
        if ctx.timestamps {
            let info = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP)
                .query_count(2);
            // SAFETY: as above.
            k.queries = unsafe { d.create_query_pool(&info, None) }.ok();
        }
        Ok(k)
    }

    /// A descriptor set of layout `which` with `buffers` at bindings 0, 1, ...
    pub fn set(&self, which: usize, buffers: &[vk::Buffer]) -> Result<vk::DescriptorSet, GpuError> {
        let layouts = [self.set_layouts[which]];
        let info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.pool)
            .set_layouts(&layouts);
        // SAFETY: a pool and layout of this device.
        let set = unsafe { self.ctx.device.allocate_descriptor_sets(&info) }
            .ctx("vkAllocateDescriptorSets")?[0];
        self.write_set(set, buffers);
        Ok(set)
    }

    /// Point `set`'s bindings at `buffers` (the set is not in use).
    pub fn write_set(&self, set: vk::DescriptorSet, buffers: &[vk::Buffer]) {
        let infos: Vec<[vk::DescriptorBufferInfo; 1]> = buffers
            .iter()
            .map(|&b| {
                [vk::DescriptorBufferInfo {
                    buffer: b,
                    offset: 0,
                    range: vk::WHOLE_SIZE,
                }]
            })
            .collect();
        let writes: Vec<vk::WriteDescriptorSet> = infos
            .iter()
            .enumerate()
            .map(|(i, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(i as u32)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(info)
            })
            .collect();
        // SAFETY: valid writes to a set not in use.
        unsafe { self.ctx.device.update_descriptor_sets(&writes, &[]) };
    }

    /// Give a set back to the pool.
    pub fn free_set(&self, set: vk::DescriptorSet) {
        // SAFETY: the set is not in use (the owner waited on the fence).
        let _ = unsafe { self.ctx.device.free_descriptor_sets(self.pool, &[set]) };
    }

    /// Record `plan` into the command buffer, submit it and wait for it; returns the GPU
    /// time (timestamp queries). `stats_buf` is the statistics buffer, `dev_in` the device
    /// input buffer of `plan.copy_in`.
    pub fn run(
        &self,
        plan: &Plan,
        stats_buf: vk::Buffer,
        dev_in: Option<vk::Buffer>,
    ) -> Result<Option<Duration>, GpuError> {
        let d = &self.ctx.device;
        let cmd = self.cmd;
        let mut gpu_time = None;
        // SAFETY: the command buffer is idle (the last frame's fence was waited on); every
        // handle recorded is alive until the wait below returns.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .ctx("vkResetCommandBuffer")?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            d.begin_command_buffer(cmd, &begin)
                .ctx("vkBeginCommandBuffer")?;
            if let Some(q) = self.queries {
                d.cmd_reset_query_pool(cmd, q, 0, 2);
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, q, 0);
            }
            let mut transfer = false;
            if let Some((_, hist, hist_bytes, _, _)) = plan.stats {
                d.cmd_fill_buffer(cmd, stats_buf, hist, hist_bytes, 0);
                transfer = true;
            }
            if let Some((src, offset, size)) = plan.copy_in {
                let dst = dev_in.expect("a device input buffer");
                let region = vk::BufferCopy {
                    src_offset: offset,
                    dst_offset: 0,
                    size,
                };
                d.cmd_copy_buffer(cmd, src, dst, &[region]);
                transfer = true;
            }
            if transfer {
                barrier(
                    d,
                    cmd,
                    (
                        vk::PipelineStageFlags::TRANSFER,
                        vk::AccessFlags::TRANSFER_WRITE,
                    ),
                    (
                        vk::PipelineStageFlags::COMPUTE_SHADER,
                        vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                    ),
                );
            }
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.layout,
                0,
                &plan.sets,
                &[],
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, plan.pipeline);
            d.cmd_dispatch(cmd, plan.groups.0, plan.groups.1, 1);
            if let Some(((zx, zy), ..)) = plan.stats {
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.stats);
                d.cmd_dispatch(cmd, zx, zy, 1);
            }
            let copies =
                plan.copy_out.is_some() || plan.stats.is_some_and(|(.., host)| host.is_some());
            if copies {
                barrier(
                    d,
                    cmd,
                    (
                        vk::PipelineStageFlags::COMPUTE_SHADER,
                        vk::AccessFlags::SHADER_WRITE,
                    ),
                    (
                        vk::PipelineStageFlags::TRANSFER,
                        vk::AccessFlags::TRANSFER_READ,
                    ),
                );
                if let Some((src, dst, size)) = plan.copy_out {
                    let region = vk::BufferCopy {
                        src_offset: 0,
                        dst_offset: 0,
                        size,
                    };
                    d.cmd_copy_buffer(cmd, src, dst, &[region]);
                }
                if let Some((.., size, Some(host))) = plan.stats {
                    let region = vk::BufferCopy {
                        src_offset: 0,
                        dst_offset: 0,
                        size,
                    };
                    d.cmd_copy_buffer(cmd, stats_buf, host, &[region]);
                }
            }
            barrier(
                d,
                cmd,
                (
                    vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                    vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE,
                ),
                (vk::PipelineStageFlags::HOST, vk::AccessFlags::HOST_READ),
            );
            if let Some(q) = self.queries {
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::BOTTOM_OF_PIPE, q, 1);
            }
            d.end_command_buffer(cmd).ctx("vkEndCommandBuffer")?;
            let cmds = [cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            {
                let queue = self.ctx.queue.lock().unwrap_or_else(|e| e.into_inner());
                d.queue_submit(*queue, &submit, self.fence)
                    .ctx("vkQueueSubmit")?;
            }
            let waited = d.wait_for_fences(&[self.fence], true, u64::MAX);
            d.reset_fences(&[self.fence]).ctx("vkResetFences")?;
            waited.ctx("vkWaitForFences")?;
            if let Some(q) = self.queries {
                let mut t = [0u64; 2];
                if d.get_query_pool_results(q, 0, &mut t, vk::QueryResultFlags::TYPE_64)
                    .is_ok()
                {
                    let ns =
                        t[1].wrapping_sub(t[0]) as f64 * self.ctx.limits.timestamp_period as f64;
                    gpu_time = Some(Duration::from_nanos(ns as u64));
                }
            }
        }
        Ok(gpu_time)
    }
}

/// What one frame's command buffer does.
pub(crate) struct Plan {
    pub sets: [vk::DescriptorSet; 3],
    pub pipeline: vk::Pipeline,
    pub groups: (u32, u32),
    /// Source buffer, offset and bytes copied into the device input buffer.
    pub copy_in: Option<(vk::Buffer, u64, u64)>,
    /// Device output buffer to the target buffer, bytes.
    pub copy_out: Option<(vk::Buffer, vk::Buffer, u64)>,
    /// Zones, histogram offset and bytes, statistics bytes, host copy.
    #[allow(clippy::type_complexity)]
    pub stats: Option<((u32, u32), u64, u64, u64, Option<vk::Buffer>)>,
}

/// A global memory barrier.
///
/// # Safety
/// `cmd` is recording.
unsafe fn barrier(
    d: &ash::Device,
    cmd: vk::CommandBuffer,
    src: (vk::PipelineStageFlags, vk::AccessFlags),
    dst: (vk::PipelineStageFlags, vk::AccessFlags),
) {
    let m = [vk::MemoryBarrier::default()
        .src_access_mask(src.1)
        .dst_access_mask(dst.1)];
    // SAFETY: the caller's.
    unsafe {
        d.cmd_pipeline_barrier(
            cmd,
            src.0,
            dst.0,
            vk::DependencyFlags::empty(),
            &m,
            &[],
            &[],
        )
    };
}
