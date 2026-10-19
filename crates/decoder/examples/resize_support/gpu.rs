use anyhow::{Context, Result};
use image::{DynamicImage, RgbaImage};
use std::{sync::mpsc, time::Duration};
use wgpu::util::DeviceExt;

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    horizontal: wgpu::ComputePipeline,
    vertical: wgpu::ComputePipeline,
    pub info: String,
}
impl Gpu {
    pub fn new() -> Result<Self> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))?;
        let info = adapter.get_info();
        anyhow::ensure!(
            info.device_type != wgpu::DeviceType::Cpu,
            "software adapter is not a GPU benchmark"
        );
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;
        let entries: Vec<_> = (0..4)
            .map(|binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: if binding == 3 {
                        wgpu::BufferBindingType::Uniform
                    } else {
                        wgpu::BufferBindingType::Storage {
                            read_only: binding == 0,
                        }
                    },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            })
            .collect();
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &entries,
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("antialiased premultiplied triangle"),
            source: wgpu::ShaderSource::Wgsl(include_str!("triangle.wgsl").into()),
        });
        let pipeline = |entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: None,
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let horizontal = pipeline("horizontal");
        let vertical = pipeline("vertical");
        Ok(Self {
            device,
            queue,
            layout,
            horizontal,
            vertical,
            info: format!("{info:?}"),
        })
    }
    /// Includes conversion, allocations, upload, both compute passes, submission,
    /// GPU completion wait, staging map and owned CPU output. No resident-input shortcut.
    pub fn resize(&self, image: DynamicImage, dw: u32, dh: u32) -> Result<DynamicImage> {
        let source = image.into_rgba8();
        let (sw, sh) = source.dimensions();
        let input = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: &source,
                usage: wgpu::BufferUsages::STORAGE,
            });
        let buffer = |size, usage| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let intermediate = buffer(
            u64::from(dw) * u64::from(sh) * 16,
            wgpu::BufferUsages::STORAGE,
        );
        let bytes = u64::from(dw) * u64::from(dh) * 4;
        let output = buffer(
            bytes,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        );
        let staging = buffer(
            bytes,
            wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        );
        let dims: Vec<u8> = [sw, sh, dw, dh]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        let uniform = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: &dims,
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let entries: Vec<_> = [&input, &intermediate, &output, &uniform]
            .iter()
            .enumerate()
            .map(|(binding, buffer)| wgpu::BindGroupEntry {
                binding: binding as u32,
                resource: buffer.as_entire_binding(),
            })
            .collect();
        let bindings = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &entries,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        for (pipeline, height) in [(&self.horizontal, sh), (&self.vertical, dh)] {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bindings, &[]);
            pass.dispatch_workgroups(dw.div_ceil(8), height.div_ceil(8), 1);
        }
        encoder.copy_buffer_to_buffer(&output, 0, &staging, 0, bytes);
        let submission = self.queue.submit([encoder.finish()]);
        let (sender, receiver) = mpsc::sync_channel(1);
        staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        self.device.poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(Duration::from_secs(30)),
        })?;
        receiver
            .recv_timeout(Duration::from_secs(1))
            .context("GPU readback callback timeout")??;
        let data = staging.slice(..).get_mapped_range()?.to_vec();
        staging.unmap();
        Ok(DynamicImage::ImageRgba8(
            RgbaImage::from_raw(dw, dh, data).context("GPU pixel length")?,
        ))
    }
}
