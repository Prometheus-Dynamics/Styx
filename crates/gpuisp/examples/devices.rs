//! Lists the Vulkan devices the GPU ISP can run on.

fn main() {
    let list = styx_gpuisp::devices();
    if list.is_empty() {
        println!("no Vulkan device suits the GPU ISP");
    }
    for d in list {
        println!(
            "{}: {} ({:?}, {}, Vulkan {}.{}.{}), dma-buf {}, separate memory {}",
            d.index,
            d.name,
            d.kind,
            d.driver,
            d.api.0,
            d.api.1,
            d.api.2,
            d.dmabuf,
            d.separate_memory
        );
    }
}
