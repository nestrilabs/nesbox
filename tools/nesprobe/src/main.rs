//! nesprobe -- a calibrated, headless GPU load probe.
//!
//! Renders a fullscreen triangle into an offscreen attachment, over and over,
//! with a fragment shader whose cost is set by a push constant. No swapchain, no
//! surface, no compositor: `VK_KHR_display` cannot work in a native-context guest
//! and going through a compositor would drag present pacing and dmabuf export --
//! things we are trying to measure -- into the measurement itself.
//!
//! It reports frames. Occupancy comes from the host, out of `drm-engine-gfx` in
//! the VMM's fdinfo. Frame count and occupancy together give GPU time per frame.

use ash::{vk, Device, Entry, Instance};
use std::ffi::CStr;
use std::time::{Duration, Instant};

struct Args {
    cost: u32,
    width: u32,
    height: u32,
    seconds: u64,
    fps: u32,
    device: usize,
    /// Seconds of frames to discard before recording anything.
    ///
    /// Not optional in practice. An idle AMD GPU sits in a low DPM state -- on
    /// the reference host `pp_dpm_sclk` idles at 400 MHz against a 2000 MHz top
    /// state -- and takes a few seconds under load to reach full clocks,
    /// measured here as 716 -> 1100 -> 2000 MHz over ~2.5 s. Frames rendered
    /// during that ramp are several times slower than steady state, and there
    /// are enough of them to *be* the p99: a 25 s run at ~90 fps is ~2200
    /// frames, of which ~120 fall in the ramp -- 5%, well above the 1% mark.
    ///
    /// So a p99 measured without this is a measurement of the clock ramp, not of
    /// the stack. Discarding one frame is not enough; the window is seconds long.
    warmup: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self { cost: 200, width: 1920, height: 1080, seconds: 20, fps: 0, device: 0, warmup: 5 }
    }
}

fn parse_args() -> Args {
    let mut a = Args::default();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let val = |i: usize| -> u64 {
            argv.get(i + 1)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| { eprintln!("{} needs a number", argv[i]); std::process::exit(2) })
        };
        match argv[i].as_str() {
            "--cost" => { a.cost = val(i) as u32; i += 2 }
            "--width" => { a.width = val(i) as u32; i += 2 }
            "--height" => { a.height = val(i) as u32; i += 2 }
            "--seconds" => { a.seconds = val(i); i += 2 }
            "--fps" => { a.fps = val(i) as u32; i += 2 }
            "--device" => { a.device = val(i) as usize; i += 2 }
            "--warmup" => { a.warmup = val(i); i += 2 }
            "-h" | "--help" => {
                println!("nesprobe [--cost N] [--width W] [--height H] [--seconds S]");
                println!("         [--fps F] [--device N] [--warmup S]");
                std::process::exit(0)
            }
            other => { eprintln!("unknown argument: {other}"); std::process::exit(2) }
        }
    }
    a
}

fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    want: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        type_bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(want)
    })
}

/// Everything Vulkan, torn down in Drop order by explicit destroy at the end.
struct Probe {
    _entry: Entry,
    instance: Instance,
    device: Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
    framebuffer: vk::Framebuffer,
    view: vk::ImageView,
    image: vk::Image,
    memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    device_name: String,
}

impl Probe {
    unsafe fn new(args: &Args) -> Result<Self, Box<dyn std::error::Error>> {
        let entry = Entry::load()?;

        let app_name = CStr::from_bytes_with_nul(b"nesprobe\0")?;
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .api_version(vk::make_api_version(0, 1, 1, 0));
        let instance =
            entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app_info), None)?;

        let physical_devices = instance.enumerate_physical_devices()?;
        if physical_devices.is_empty() {
            return Err("no Vulkan physical devices -- is the driver present?".into());
        }
        let pd = *physical_devices
            .get(args.device)
            .ok_or_else(|| format!("--device {} out of range ({} present)", args.device, physical_devices.len()))?;

        let props = instance.get_physical_device_properties(pd);
        let device_name = CStr::from_ptr(props.device_name.as_ptr()).to_string_lossy().into_owned();

        // A queue that can do graphics. Nothing else is required.
        let qfam = instance
            .get_physical_device_queue_family_properties(pd)
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or("no graphics queue family")? as u32;

        let prios = [1.0f32];
        let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(qfam).queue_priorities(&prios)];
        let device = instance.create_device(pd, &vk::DeviceCreateInfo::default().queue_create_infos(&qci), None)?;
        let queue = device.get_device_queue(qfam, 0);

        let extent = vk::Extent2D { width: args.width, height: args.height };
        let format = vk::Format::R8G8B8A8_UNORM;

        let image = device.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D { width: extent.width, height: extent.height, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )?;

        let req = device.get_image_memory_requirements(image);
        let mem_props = instance.get_physical_device_memory_properties(pd);
        let mem_type = find_memory_type(&mem_props, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .ok_or("no DEVICE_LOCAL memory type for the render target")?;
        let memory = device.allocate_memory(
            &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(mem_type),
            None,
        )?;
        device.bind_image_memory(image, memory, 0)?;

        let view = device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                }),
            None,
        )?;

        // The attachment is cleared at load and left in COLOR_ATTACHMENT_OPTIMAL:
        // nothing ever reads it back, so there is no transition to pay for.
        let attachments = [vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let color_refs = [vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        }];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_refs)];
        let render_pass = device.create_render_pass(
            &vk::RenderPassCreateInfo::default().attachments(&attachments).subpasses(&subpasses),
            None,
        )?;

        let fb_views = [view];
        let framebuffer = device.create_framebuffer(
            &vk::FramebufferCreateInfo::default()
                .render_pass(render_pass)
                .attachments(&fb_views)
                .width(extent.width)
                .height(extent.height)
                .layers(1),
            None,
        )?;

        let vert = read_spv(include_bytes!("../shaders/probe.vert.spv"))?;
        let frag = read_spv(include_bytes!("../shaders/probe.frag.spv"))?;
        let vs = device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vert), None)?;
        let fs = device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&frag), None)?;

        let push_ranges = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::FRAGMENT,
            offset: 0,
            size: 4,
        }];
        let layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push_ranges),
            None,
        )?;

        let entry_name = CStr::from_bytes_with_nul(b"main\0")?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vs)
                .name(entry_name),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fs)
                .name(entry_name),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent }];
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewports(&viewports)
            .scissors(&scissors);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);

        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);
        let pipeline = device
            .create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
            .map_err(|(_, e)| e)?[0];

        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(fs, None);

        let pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(qfam)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        let cmd = device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )?[0];
        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;

        Ok(Self {
            _entry: entry, instance, device, queue, pool, cmd, fence, pipeline, layout,
            render_pass, framebuffer, view, image, memory, extent, device_name,
        })
    }

    /// One frame: clear, draw a fullscreen triangle, wait for the GPU.
    unsafe fn frame(&self, cost: u32) -> Result<(), vk::Result> {
        let d = &self.device;
        d.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())?;
        d.begin_command_buffer(
            self.cmd,
            &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;

        let clears = [vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] } }];
        d.cmd_begin_render_pass(
            self.cmd,
            &vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                .framebuffer(self.framebuffer)
                .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: self.extent })
                .clear_values(&clears),
            vk::SubpassContents::INLINE,
        );
        d.cmd_bind_pipeline(self.cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
        d.cmd_push_constants(
            self.cmd,
            self.layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            &cost.to_ne_bytes(),
        );
        d.cmd_draw(self.cmd, 3, 1, 0, 0);
        d.cmd_end_render_pass(self.cmd);
        d.end_command_buffer(self.cmd)?;

        let cmds = [self.cmd];
        let submits = [vk::SubmitInfo::default().command_buffers(&cmds)];
        d.reset_fences(&[self.fence])?;
        d.queue_submit(self.queue, &submits, self.fence)?;
        d.wait_for_fences(&[self.fence], true, u64::MAX)?;
        Ok(())
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.layout, None);
            self.device.destroy_framebuffer(self.framebuffer, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn read_spv(bytes: &[u8]) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
    if bytes.len() % 4 != 0 {
        return Err("SPIR-V length is not a multiple of 4".into());
    }
    Ok(bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

fn main() {
    let args = parse_args();
    let probe = match unsafe { Probe::new(&args) } {
        Ok(p) => p,
        Err(e) => { eprintln!("nesprobe: {e}"); std::process::exit(1) }
    };

    println!(
        "nesprobe: {} | {}x{} | cost={} | fps={} | {}s",
        probe.device_name,
        args.width,
        args.height,
        args.cost,
        if args.fps == 0 { "unpaced".to_string() } else { args.fps.to_string() },
        if args.seconds == 0 { "until killed".to_string() } else { args.seconds.to_string() }
    );

    // One warm-up frame, discarded: it carries pipeline compilation and first-touch
    // allocation. Note this is not sufficient on its own -- the GPU also needs
    // several seconds to reach steady clocks, see docs/BENCHMARKS.md.
    if let Err(e) = unsafe { probe.frame(args.cost) } {
        eprintln!("nesprobe: first frame failed: {e}");
        std::process::exit(1);
    }

    let interval = if args.fps > 0 { Some(Duration::from_secs_f64(1.0 / args.fps as f64)) } else { None };
    let start = Instant::now();
    let deadline = if args.seconds == 0 { None } else { Some(start + Duration::from_secs(args.seconds)) };

    let warmup_until = start + Duration::from_secs(args.warmup);
    let mut frames: u64 = 0;
    let mut discarded: u64 = 0;
    let mut frame_times: Vec<f64> = Vec::with_capacity(4096);
    let mut window_start = Instant::now();
    let mut window_frames: u64 = 0;
    let mut next_frame = Instant::now();

    loop {
        if let Some(d) = deadline {
            if Instant::now() >= d { break }
        }
        let t0 = Instant::now();
        if let Err(e) = unsafe { probe.frame(args.cost) } {
            eprintln!("nesprobe: frame {frames} failed: {e}");
            break;
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if Instant::now() < warmup_until {
            discarded += 1;
        } else {
            frame_times.push(ms);
            frames += 1;
            window_frames += 1;
        }

        if let Some(iv) = interval {
            next_frame += iv;
            let now = Instant::now();
            if next_frame > now { std::thread::sleep(next_frame - now) } else { next_frame = now }
        }

        let w = window_start.elapsed();
        if w >= Duration::from_secs(1) && window_frames > 0 {
            println!(
                "  t={:5.1}s  frames={:6}  fps={:7.2}  frame_ms_mean={:7.3}",
                start.elapsed().as_secs_f64(),
                frames,
                window_frames as f64 / w.as_secs_f64(),
                frame_times[frame_times.len() - window_frames as usize..].iter().sum::<f64>()
                    / window_frames as f64
            );
            window_start = Instant::now();
            window_frames = 0;
        }
    }

    let elapsed = start.elapsed().as_secs_f64() - args.warmup as f64;
    frame_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = frame_times.iter().sum::<f64>() / frame_times.len().max(1) as f64;
    let pct = |p: f64| -> f64 {
        if frame_times.is_empty() { return 0.0 }
        frame_times[((frame_times.len() as f64 * p) as usize).min(frame_times.len() - 1)]
    };

    println!("---");
    println!("warmup        {} s, {discarded} frames discarded", args.warmup);
    println!("frames        {frames}");
    println!("elapsed       {elapsed:.3} s");
    println!("fps           {:.2}", frames as f64 / elapsed);
    // Latency, not occupancy: this is submit-to-fence wall time, and with more than
    // one guest on the card it includes queueing behind another guest. Occupancy
    // comes from drm-engine-gfx on the host. Both are reported because the gap
    // between them is itself the interference signal.
    println!("frame_ms      mean {mean:.3}  p50 {:.3}  p99 {:.3}  max {:.3}", pct(0.50), pct(0.99), pct(1.0));
}
