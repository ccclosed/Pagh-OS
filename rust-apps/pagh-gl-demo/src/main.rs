//! CLI-биндер демо: headless-рендер или живой PDP/1-клиент.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::Duration;

use pagh_gl_demo::cli::{self, Options};
use pagh_gl_demo::client::{Event, PdpClient};
use pagh_gl_demo::render::{render_headless, GlRenderer};
use pagh_gl_demo::timing::{Series, Stopwatch};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match cli::parse(&args) {
        Ok(options) => options,
        Err(message) if message.is_empty() => {
            println!("{}", cli::usage());
            return;
        }
        Err(message) => {
            eprintln!("pagh-gl-demo: {message}\n\n{}", cli::usage());
            std::process::exit(2);
        }
    };
    if let Err(error) = run(&options) {
        eprintln!("pagh-gl-demo: {error}");
        std::process::exit(1);
    }
}

fn run(options: &Options) -> Result<(), Box<dyn Error>> {
    match options.connect.as_deref() {
        Some(socket) => run_live(options, socket),
        None => run_headless(options),
    }
}

fn run_headless(options: &Options) -> Result<(), Box<dyn Error>> {
    let scene = options.scene;
    let keep_all = options.out.is_some();
    let frames = render_headless(
        scene,
        options.width,
        options.height,
        options.frames,
        options.lighting,
        keep_all,
    )?;

    if let Some(directory) = &options.out {
        fs::create_dir_all(directory)?;
        for (index, pixels) in frames.all.iter().enumerate() {
            let path = directory.join(format!("pagh-gl-demo-{}-frame{index}.ppm", scene.name()));
            write_ppm(&path, options.width, options.height, pixels)?;
            eprintln!("pagh-gl-demo: wrote {}", path.display());
        }
    }

    for (index, digest) in frames.digests.iter().enumerate() {
        // Без --hash печатаем все кадры тоже: headless — это и есть pixel
        // check, и строка на кадр — единственный наблюдаемый результат.
        println!(
            "TINYGL {} frame={index} digest=0x{digest:016x} w={} h={}",
            scene.name(),
            options.width,
            options.height
        );
    }
    print_summary(scene.name(), &frames.wall, &frames.cycles);
    Ok(())
}

fn run_live(options: &Options, socket: &str) -> Result<(), Box<dyn Error>> {
    let scene = options.scene;
    let (mut client, mut frame) = PdpClient::connect(
        socket,
        "pagh-gl-demo",
        options.width,
        options.height,
        &format!("TinyGL — {}", scene.name()),
    )?;
    let (width, height) = client.size();
    eprintln!(
        "pagh-gl-demo: connected to {socket}, window {} {width}x{height}, generation {}",
        client.window_id(),
        client.generation()
    );
    let mut renderer = GlRenderer::new(&mut frame)?;
    let mut wall = Series::default();
    let mut cycles = Series::default();
    let mut commit_wall = Series::default();
    let mut commit_cycles = Series::default();
    let mut rendered: u32 = 0;
    let mut last_pixels: Option<Vec<u32>> = None;
    let mut closed_by_server = None;

    loop {
        // События: короткий poll, чтобы живое окно реагировало на configure,
        // close и release, но кадры по-прежнему считались по номеру, а не по
        // времени.
        for event in client.poll(Duration::from_millis(2))? {
            match event {
                Event::Configure {
                    width: new_width,
                    height: new_height,
                } => {
                    if (new_width, new_height) == (renderer.width(), renderer.height()) {
                        continue;
                    }
                    if new_width % 4 != 0 {
                        return Err(format!(
                            "композитор сконфигурировал ширину {new_width}: TinyGL требует кратность 4"
                        )
                        .into());
                    }
                    if new_width == 0 || new_height == 0 {
                        continue;
                    }
                    drop(renderer);
                    frame = client.resize(new_width, new_height)?;
                    renderer = GlRenderer::new(&mut frame)?;
                    eprintln!(
                        "pagh-gl-demo: resize -> {}x{} (generation {})",
                        new_width,
                        new_height,
                        client.generation()
                    );
                }
                Event::ShmReleased { generation } => {
                    eprintln!(
                        "pagh-gl-demo: композитор освободил generation {generation}; повторный SHM_OFFER"
                    );
                    let (width, height) = (renderer.width(), renderer.height());
                    drop(renderer);
                    frame = client.resize(width, height)?;
                    renderer = GlRenderer::new(&mut frame)?;
                }
                Event::Close { reason } => {
                    closed_by_server = Some(reason);
                    break;
                }
                Event::Error { code, message } => {
                    eprintln!("pagh-gl-demo: композитор: error {code}: {message}");
                }
            }
        }
        if closed_by_server.is_some() {
            break;
        }

        let watch = Stopwatch::start();
        renderer.render(scene, rendered, options.lighting)?;
        let (frame_wall, frame_cycles) = watch.stop();
        wall.push(frame_wall.as_secs_f64() * 1000.0);
        cycles.push(frame_cycles as f64);
        let digest = pagh_gl_demo::pixel_digest(renderer.pixels());

        let commit_watch = Stopwatch::start();
        client.commit()?;
        let (sent_wall, sent_cycles) = commit_watch.stop();
        commit_wall.push(sent_wall.as_secs_f64() * 1000.0);
        commit_cycles.push(sent_cycles as f64);

        if options.hash || rendered == 0 || rendered % 60 == 59 {
            println!(
                "TINYGL {} frame={rendered} digest=0x{digest:016x} w={} h={}",
                scene.name(),
                renderer.width(),
                renderer.height()
            );
        }
        if rendered % 60 == 59 {
            // Живой прогон может не дойти до конца (окно закроют/снимут
            // скриншот) — печатаем накопительный ft периодически, а не только
            // на выходе.
            print_summary(scene.name(), &wall, &cycles);
        }
        if options.out.is_some() {
            last_pixels = Some(renderer.pixels().to_vec());
        }

        rendered = rendered.wrapping_add(1);
        if options.frames > 0 && rendered >= options.frames {
            break;
        }
    }

    if let (Some(directory), Some(pixels)) = (&options.out, last_pixels.as_ref()) {
        fs::create_dir_all(directory)?;
        let path = directory.join(format!("pagh-gl-demo-{}-live.ppm", scene.name()));
        write_ppm(&path, renderer.width(), renderer.height(), pixels)?;
        eprintln!("pagh-gl-demo: wrote {}", path.display());
    }

    print_summary(scene.name(), &wall, &cycles);
    let (cmin, cmed, cmax) = commit_wall.summary();
    let (cymin, cymed, cymax) = commit_cycles.summary();
    println!(
        "TINYGL {} commit={cmin:.3}/{cmed:.3}/{cmax:.3} ms cycles={cymin:.0}/{cymed:.0}/{cymax:.0}",
        scene.name()
    );

    drop(renderer);
    client.close();
    if let Some(reason) = closed_by_server {
        eprintln!(
            "pagh-gl-demo: окно закрыто композитором (reason {reason}); отрисовано {rendered} кадров"
        );
    }
    Ok(())
}

fn print_summary(scene: &str, wall: &Series, cycles: &Series) {
    let (min, med, max) = wall.summary();
    let (cmin, cmed, cmax) = cycles.summary();
    println!(
        "TINYGL {scene} ft={min:.3}/{med:.3}/{max:.3} ms (wall) \
         cycles={cmin:.0}/{cmed:.0}/{cmax:.0}"
    );
}

/// PPM P6 (RGB) из `0x00RRGGBB`-пикселей TinyGL. Формат без зависимостей и
/// открывается любым просмотрщиком; PNG-энкодер композитора сюда не тянем.
fn write_ppm(path: &Path, width: u32, height: u32, pixels: &[u32]) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(15 + (width as usize) * (height as usize) * 3);
    bytes.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    for pixel in pixels.iter().take((width as usize) * (height as usize)) {
        bytes.push((pixel >> 16) as u8);
        bytes.push((pixel >> 8) as u8);
        bytes.push(*pixel as u8);
    }
    fs::write(path, bytes)
}
