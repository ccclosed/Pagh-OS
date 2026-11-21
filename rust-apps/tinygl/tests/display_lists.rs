//! PR3: display lists как способ батчинга — равенство immediate-рендеру,
//! compile/execute режимы, вложенность, эмуляция `glDeleteLists` и отказы на
//! входах, которые уронили бы C (`assert`/`fatal`/`NULL`).
//!
//! Тесты с `Context` сериализуются: `gl_ctx` в TinyGL глобальный.

use std::sync::{Mutex, MutexGuard};

use tinygl::{
    pixel_digest, Context, FbError, ListMode, MatrixMode, Primitive, Surface, TextureEnvMode,
    TextureParam, TexturePname,
};

const W: u32 = 64;
const H: u32 = 64;
const CLEAR: u32 = 0x0010_2030;

static GL_TEST_LOCK: Mutex<()> = Mutex::new(());

fn gl_test_lock() -> MutexGuard<'static, ()> {
    GL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

struct Frame {
    digest: u64,
    pixels: Vec<u32>,
}

fn frame(clear_color: u32, draw: impl FnOnce(&Context<'_>)) -> Frame {
    let mut fb = vec![0u32; (W * H) as usize];
    {
        let surface = Surface::new(&mut fb, W, H).expect("64x64 surface");
        let ctx = Context::new(surface).expect("single live context");
        ctx.set_viewport(0, 0, W, H).expect("valid viewport");
        ctx.clear(clear_color, 0);
        draw(&ctx);
    }
    Frame {
        digest: pixel_digest(&fb),
        pixels: fb,
    }
}

fn reference_projection(ctx: &Context<'_>) {
    ctx.matrix_mode(MatrixMode::Projection);
    ctx.load_identity();
    ctx.frustum(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
        .expect("reference frustum");
    ctx.matrix_mode(MatrixMode::ModelView);
    ctx.load_identity();
    ctx.translate(0.0, 0.0, -3.0).expect("finite translate");
}

fn quadrant_rgb() -> Vec<u8> {
    let mut data = Vec::with_capacity(8 * 8 * 3);
    for y in 0..8u32 {
        for x in 0..8u32 {
            let (r, g, b) = match (x >= 4, y >= 4) {
                (false, false) => (255, 0, 0),
                (true, false) => (0, 255, 0),
                (false, true) => (0, 0, 255),
                (true, true) => (255, 255, 255),
            };
            data.extend_from_slice(&[r, g, b]);
        }
    }
    data
}

fn bind_quadrant_texture(ctx: &Context<'_>) -> u32 {
    let names = ctx.gen_textures(1).expect("texture name");
    ctx.tex_env(TextureEnvMode::Decal);
    ctx.tex_parameter(TexturePname::WrapS, TextureParam::Repeat)
        .expect("wrap S");
    ctx.tex_parameter(TexturePname::WrapT, TextureParam::Repeat)
        .expect("wrap T");
    ctx.bind_texture(names[0]).expect("bind");
    ctx.tex_image_2d(8, 8, &quadrant_rgb()).expect("upload");
    names[0]
}

fn draw_textured_quad(ctx: &Context<'_>) {
    ctx.begin(Primitive::Quads).expect("begin");
    ctx.tex_coord2f(0.0, 0.0);
    ctx.vertex3f(-1.0, -1.0, 0.0);
    ctx.tex_coord2f(1.0, 0.0);
    ctx.vertex3f(1.0, -1.0, 0.0);
    ctx.tex_coord2f(1.0, 1.0);
    ctx.vertex3f(1.0, 1.0, 0.0);
    ctx.tex_coord2f(0.0, 1.0);
    ctx.vertex3f(-1.0, 1.0, 0.0);
    ctx.end().expect("end");
}

fn draw_untextured_triangle(ctx: &Context<'_>, rgb: (f32, f32, f32)) {
    ctx.begin(Primitive::Triangles).expect("begin");
    ctx.color3f(rgb.0, rgb.1, rgb.2);
    ctx.vertex3f(-1.0, -1.0, 0.0);
    ctx.vertex3f(1.0, -1.0, 0.0);
    ctx.vertex3f(0.0, 1.0, 0.0);
    ctx.end().expect("end");
}

/// Immediate-кадр: текстура включена вручную.
fn render_immediate_textured() -> Frame {
    frame(CLEAR, |ctx| {
        reference_projection(ctx);
        bind_quadrant_texture(ctx);
        ctx.enable_texture_2d().expect("enable texture");
        draw_textured_quad(ctx);
        ctx.disable_texture_2d();
    })
}

/// Тот же кадр, но геометрия и enable/disable записаны в display list.
fn render_list_textured(mode: ListMode) -> Frame {
    frame(CLEAR, |ctx| {
        reference_projection(ctx);
        bind_quadrant_texture(ctx);
        let list = ctx.gen_lists(1).expect("list name");
        ctx.new_list(list, mode).expect("new list");
        ctx.enable_texture_2d().expect("recorded enable");
        draw_textured_quad(ctx);
        ctx.disable_texture_2d();
        ctx.end_list().expect("end list");
        ctx.call_list(list).expect("replay list");
    })
}

// --- равенство immediate и display list ----------------------------------

#[test]
fn display_list_render_equals_immediate_render() {
    let _guard = gl_test_lock();
    let immediate = render_immediate_textured();
    for mode in [ListMode::Compile, ListMode::CompileAndExecute] {
        let listed = render_list_textured(mode);
        assert_eq!(
            listed.digest, immediate.digest,
            "{mode:?} display-list digest must equal the immediate-mode digest"
        );
        assert_eq!(
            listed.pixels, immediate.pixels,
            "{mode:?} display-list pixels must be identical to immediate mode"
        );
    }
    assert_ne!(immediate.digest, 0);
}

#[test]
fn compile_and_execute_executes_then_replays() {
    let _guard = gl_test_lock();
    // Эталон: один и тот же треугольник в двух положениях (список хранит
    // одну геометрию, поэтому и immediate рисует её же).
    let immediate = frame(CLEAR, |ctx| {
        reference_projection(ctx);
        draw_untextured_triangle(ctx, (1.0, 0.5, 0.0));
        ctx.translate(0.0, 0.4, 0.0).expect("translate");
        draw_untextured_triangle(ctx, (1.0, 0.5, 0.0));
    });
    // GL_COMPILE_AND_EXECUTE: список исполнился при записи (первое
    // положение) и ещё раз по call_list после сдвига.
    let listed = frame(CLEAR, |ctx| {
        reference_projection(ctx);
        let list = ctx.gen_lists(1).expect("list name");
        ctx.new_list(list, ListMode::CompileAndExecute)
            .expect("new C&E list");
        draw_untextured_triangle(ctx, (1.0, 0.5, 0.0));
        ctx.end_list().expect("end list");
        ctx.translate(0.0, 0.4, 0.0).expect("translate");
        ctx.call_list(list).expect("replay");
    });
    assert_eq!(listed.digest, immediate.digest);
    assert_eq!(listed.pixels, immediate.pixels);
}

#[test]
fn nested_list_call_matches_immediate() {
    let _guard = gl_test_lock();
    let immediate = frame(CLEAR, |ctx| {
        reference_projection(ctx);
        draw_untextured_triangle(ctx, (1.0, 0.0, 0.0));
        ctx.push_matrix().expect("push");
        ctx.translate(0.0, 0.4, 0.0).expect("translate");
        draw_untextured_triangle(ctx, (1.0, 0.0, 0.0));
        ctx.pop_matrix().expect("pop");
        draw_untextured_triangle(ctx, (0.0, 0.0, 1.0));
    });
    let nested = frame(CLEAR, |ctx| {
        reference_projection(ctx);
        let inner = ctx.gen_lists(1).expect("inner name");
        ctx.new_list(inner, ListMode::Compile).expect("inner");
        draw_untextured_triangle(ctx, (1.0, 0.0, 0.0));
        ctx.end_list().expect("inner end");

        let outer = ctx.gen_lists(1).expect("outer name");
        ctx.new_list(outer, ListMode::Compile).expect("outer");
        ctx.call_list(inner).expect("record inner call");
        ctx.push_matrix().expect("record push");
        ctx.translate(0.0, 0.4, 0.0).expect("record translate");
        ctx.call_list(inner).expect("record inner call again");
        ctx.pop_matrix().expect("record pop");
        draw_untextured_triangle(ctx, (0.0, 0.0, 1.0));
        ctx.end_list().expect("outer end");

        ctx.call_list(outer).expect("replay nested lists");
    });
    assert_eq!(nested.digest, immediate.digest);
    assert_eq!(nested.pixels, immediate.pixels);
}

#[test]
fn list_of_vertices_can_be_called_inside_begin() {
    let _guard = gl_test_lock();
    let immediate = frame(CLEAR, |ctx| {
        reference_projection(ctx);
        draw_untextured_triangle(ctx, (0.25, 0.5, 0.75));
    });
    let batched = frame(CLEAR, |ctx| {
        reference_projection(ctx);
        let list = ctx.gen_lists(1).expect("list name");
        ctx.new_list(list, ListMode::Compile).expect("new list");
        // Только вершины: begin/end живут снаружи, как в GL.
        ctx.vertex3f(-1.0, -1.0, 0.0);
        ctx.vertex3f(1.0, -1.0, 0.0);
        ctx.vertex3f(0.0, 1.0, 0.0);
        ctx.end_list().expect("end list");

        ctx.begin(Primitive::Triangles).expect("begin");
        ctx.color3f(0.25, 0.5, 0.75);
        ctx.call_list(list).expect("replay vertices inside begin");
        ctx.end().expect("end");
    });
    assert_eq!(batched.digest, immediate.digest);
    assert_eq!(batched.pixels, immediate.pixels);
}

#[test]
fn replay_restores_matrix_mode_and_texture_state() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");
    let names = ctx.gen_textures(1).expect("texture name");
    ctx.bind_texture(names[0]).expect("bind");
    ctx.tex_image_2d(2, 2, &vec![9u8; 12]).expect("upload");

    let list = ctx.gen_lists(1).expect("list name");
    ctx.new_list(list, ListMode::Compile).expect("new list");
    ctx.push_matrix().expect("record push");
    ctx.translate(0.5, 0.0, 0.0).expect("record translate");
    ctx.pop_matrix().expect("record pop");
    ctx.matrix_mode(MatrixMode::Texture);
    ctx.load_identity();
    ctx.translate(0.25, 0.5, 0.0)
        .expect("record texture translate");
    ctx.tex_coord2f(0.25, 0.75);
    ctx.enable_texture_2d().expect("record enable");
    ctx.disable_texture_2d();
    ctx.end_list().expect("end list");

    assert_eq!(ctx.current_matrix_mode(), MatrixMode::ModelView);
    ctx.call_list(list).expect("replay");
    assert_eq!(
        ctx.current_matrix_mode(),
        MatrixMode::Texture,
        "matrix mode changed by the list must be reflected in the live mirror"
    );
    let texture_matrix = ctx.get_matrix(MatrixMode::Texture);
    assert_eq!(
        (texture_matrix[12], texture_matrix[13], texture_matrix[14]),
        (0.25, 0.5, 0.0),
        "list must leave the C texture matrix at the recorded state"
    );
    // Список выключил текстуру в конце: живое состояние — выключено.
    ctx.enable_texture_2d()
        .expect("texture is still bound and has an image");
}

#[test]
fn display_list_can_be_defined_twice() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");
    let list = ctx.gen_lists(1).expect("list name");
    for color in [(1.0f32, 0.0, 0.0), (0.0, 1.0, 0.0)] {
        ctx.new_list(list, ListMode::Compile).expect("redefine");
        ctx.begin(Primitive::Triangles).expect("begin");
        ctx.color3f(color.0, color.1, color.2);
        ctx.vertex3f(-1.0, -1.0, 0.0);
        ctx.vertex3f(1.0, -1.0, 0.0);
        ctx.vertex3f(0.0, 1.0, 0.0);
        ctx.end().expect("end");
        ctx.end_list().expect("end list");
        assert!(ctx.is_list(list));
    }
}

// --- эмуляция glDeleteLists ----------------------------------------------

#[test]
fn delete_lists_hides_name_but_keeps_the_slot_occupied() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 16 * 16];
    let surface = Surface::new(&mut fb, 16, 16).expect("16x16");
    let ctx = Context::new(surface).expect("single context");

    let list = ctx.gen_lists(1).expect("list name");
    ctx.new_list(list, ListMode::Compile).expect("new list");
    ctx.begin(Primitive::Triangles).expect("begin");
    ctx.vertex3f(-1.0, -1.0, 0.0);
    ctx.vertex3f(1.0, -1.0, 0.0);
    ctx.vertex3f(0.0, 1.0, 0.0);
    ctx.end().expect("end");
    ctx.end_list().expect("end list");
    assert!(ctx.is_list(list));
    ctx.call_list(list).expect("call before delete");

    ctx.delete_lists(list, 1).expect("emulated delete");
    assert!(!ctx.is_list(list), "deleted name must no longer be a list");
    assert_eq!(
        ctx.call_list(list),
        Err(FbError::ListUndefined { list }),
        "calling a deleted name must be our Err, not upstream exit(1)"
    );

    // Имя можно переопределить, как в GL (слот в C всё ещё занят).
    ctx.new_list(list, ListMode::Compile).expect("redefine");
    ctx.begin(Primitive::Triangles).expect("begin");
    ctx.vertex3f(-1.0, -1.0, 0.0);
    ctx.vertex3f(1.0, -1.0, 0.0);
    ctx.vertex3f(0.0, 1.0, 0.0);
    ctx.end().expect("end");
    ctx.end_list().expect("end list");
    ctx.call_list(list).expect("call after redefine");

    // Удаление невыданных имён — no-op (как в GL), range 0 — тоже.
    ctx.delete_lists(5000, 10)
        .expect("out-of-range names ignored");
    ctx.delete_lists(0, 0).expect("range 0 ignored");
}

#[test]
fn gen_lists_finds_consecutive_runs_and_reports_exhaustion() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 4 * 4];
    let surface = Surface::new(&mut fb, 4, 4).expect("4x4");
    let ctx = Context::new(surface).expect("single context");

    assert_eq!(ctx.gen_lists(0), Err(FbError::BadListRange { range: 0 }));
    assert_eq!(
        ctx.gen_lists(1025),
        Err(FbError::BadListRange { range: 1025 })
    );

    // Upstream glGenLists(1) на пустом контексте возвращает 0 — это успех,
    // а не отказ; наш слой отличает его по собственному зеркалу.
    assert_eq!(ctx.gen_lists(1).expect("first list"), 0);
    assert_eq!(ctx.gen_lists(1).expect("second list"), 1);
    assert_eq!(ctx.gen_lists(1022).expect("rest of the slots"), 2);
    assert_eq!(
        ctx.gen_lists(1),
        Err(FbError::ListSpaceExhausted),
        "all 1024 upstream slots are taken"
    );
    assert!(ctx.is_list(0));
    assert!(ctx.is_list(1023));
}

// --- отказы на плохих входах ---------------------------------------------

fn fresh_context<'a>(fb: &'a mut [u32]) -> Context<'a> {
    let surface = Surface::new(fb, 4, 4).expect("4x4");
    Context::new(surface).expect("single context")
}

#[test]
fn list_api_rejects_bad_lifecycle() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 4 * 4];
    let ctx = fresh_context(&mut fb);

    assert_eq!(ctx.end_list(), Err(FbError::ListNotCompiling));
    assert_eq!(
        ctx.new_list(5, ListMode::Compile),
        Err(FbError::ListUndefined { list: 5 }),
        "names not issued by gen_lists would index shared_state.lists out of bounds"
    );
    assert_eq!(
        ctx.new_list(1024, ListMode::Compile),
        Err(FbError::ListUndefined { list: 1024 })
    );
    assert_eq!(ctx.call_list(7), Err(FbError::ListUndefined { list: 7 }));

    let list = ctx.gen_lists(1).expect("list name");
    ctx.new_list(list, ListMode::Compile).expect("open");
    assert_eq!(
        ctx.new_list(list, ListMode::Compile),
        Err(FbError::ListAlreadyCompiling)
    );
    // begin без end: список остаётся открытым, сначала end().
    ctx.begin(Primitive::Triangles).expect("begin");
    assert_eq!(ctx.end_list(), Err(FbError::ListBeginOpen { list }));
    ctx.end().expect("close begin");
    ctx.end_list().expect("close list");

    // new_list при открытом живом glBegin отвергается.
    ctx.begin(Primitive::Triangles).expect("begin");
    assert_eq!(
        ctx.new_list(list, ListMode::Compile),
        Err(FbError::ListDuringBegin)
    );
    ctx.end().expect("end");
}

#[test]
fn replay_rejects_crash_conditions_before_c() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 4 * 4];
    let ctx = fresh_context(&mut fb);

    // 1. Вершина без begin на верхнем уровне реплея: C дал бы assert.
    let stray = ctx.gen_lists(1).expect("stray list");
    ctx.new_list(stray, ListMode::Compile).expect("open");
    ctx.vertex3f(0.0, 0.0, 0.0);
    ctx.end_list().expect("recorded");
    assert_eq!(
        ctx.call_list(stray),
        Err(FbError::ListMalformed { list: stray })
    );

    // 2. Переполнение modelview-стека на реплее (предел 32).
    let overflow = ctx.gen_lists(1).expect("overflow list");
    ctx.new_list(overflow, ListMode::Compile).expect("open");
    for _ in 0..40 {
        ctx.push_matrix().expect("recorded push");
    }
    ctx.end_list().expect("end");
    assert_eq!(
        ctx.call_list(overflow),
        Err(FbError::MatrixStackOverflow { mode: "modelview" })
    );

    // 3. Недостача стека.
    let underflow = ctx.gen_lists(1).expect("underflow list");
    ctx.new_list(underflow, ListMode::Compile).expect("open");
    ctx.pop_matrix().expect("recorded pop");
    ctx.end_list().expect("end");
    assert_eq!(
        ctx.call_list(underflow),
        Err(FbError::MatrixStackUnderflow { mode: "modelview" })
    );

    // 4. Список с begin/end, вызванный внутри живого begin: C дал бы assert
    //    на вложенном glBegin.
    let begin_list = ctx.gen_lists(1).expect("begin list");
    ctx.new_list(begin_list, ListMode::Compile).expect("open");
    ctx.begin(Primitive::Triangles).expect("record begin");
    ctx.vertex3f(-1.0, -1.0, 0.0);
    ctx.vertex3f(1.0, -1.0, 0.0);
    ctx.vertex3f(0.0, 1.0, 0.0);
    ctx.end().expect("record end");
    ctx.end_list().expect("end");
    ctx.begin(Primitive::Lines).expect("live begin");
    assert_eq!(ctx.call_list(begin_list), Err(FbError::BeginNested));
    ctx.end().expect("live end");

    // 5. Включение текстуры без валидной текстуры: C разыменовал бы NULL.
    let texture = ctx.gen_lists(1).expect("texture list");
    ctx.new_list(texture, ListMode::Compile).expect("open");
    ctx.enable_texture_2d().expect("recorded enable");
    ctx.end_list().expect("end");
    assert_eq!(ctx.call_list(texture), Err(FbError::TextureNotBound));

    // 6. Текстура включена, затем в списке привязана текстура без картинки —
    //    текстурный треугольник упал бы на pixmap == NULL.
    let mut ctx = ctx;
    let names = ctx.gen_textures(1).expect("texture name");
    ctx.bind_texture(names[0]).expect("bind");
    ctx.tex_image_2d(2, 2, &vec![4u8; 12]).expect("upload");
    ctx.enable_texture_2d().expect("enable valid texture");
    let bad = ctx.gen_lists(1).expect("bad texture list");
    ctx.new_list(bad, ListMode::Compile).expect("open");
    ctx.bind_texture(999)
        .expect("recorded bind of untextured name");
    ctx.begin(Primitive::Triangles).expect("begin");
    ctx.vertex3f(-1.0, -1.0, 0.0);
    ctx.vertex3f(1.0, -1.0, 0.0);
    ctx.vertex3f(0.0, 1.0, 0.0);
    ctx.end().expect("end");
    ctx.end_list().expect("end list");
    assert_eq!(
        ctx.call_list(bad),
        Err(FbError::TextureHasNoImage { texture: 999 })
    );
}

#[test]
fn recursive_lists_are_rejected() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 4 * 4];
    let ctx = fresh_context(&mut fb);

    let a = ctx.gen_lists(2).expect("two names");
    let b = a + 1;

    // Прямая рекурсия: A вызывает A.
    ctx.new_list(a, ListMode::Compile).expect("open A");
    ctx.call_list(a).expect("record self call");
    ctx.end_list().expect("end A");
    assert_eq!(ctx.call_list(a), Err(FbError::ListCycle { list: a }));

    // Косвенная: A -> B -> A.
    ctx.new_list(a, ListMode::Compile).expect("redefine A");
    ctx.call_list(b).expect("record A -> B");
    ctx.end_list().expect("end A");
    ctx.new_list(b, ListMode::Compile).expect("open B");
    ctx.call_list(a).expect("record B -> A");
    ctx.end_list().expect("end B");
    assert_eq!(ctx.call_list(a), Err(FbError::ListCycle { list: a }));
}

#[test]
fn nested_matrix_mode_change_is_modelled_at_replay_time() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 4 * 4];
    let ctx = fresh_context(&mut fb);

    // inner переключает режим на GL_TEXTURE и делает push/pop; outer вызывает
    // inner и тоже push/pop. На реплее push outer обязан попасть в texture-стек,
    // а не в тот режим, что был при записи.
    let inner = ctx.gen_lists(1).expect("inner");
    ctx.new_list(inner, ListMode::Compile).expect("open inner");
    ctx.matrix_mode(MatrixMode::Texture);
    ctx.push_matrix().expect("record inner push");
    ctx.translate(0.1, 0.2, 0.0).expect("record translate");
    ctx.pop_matrix().expect("record inner pop");
    ctx.end_list().expect("end inner");

    let outer = ctx.gen_lists(1).expect("outer");
    ctx.new_list(outer, ListMode::Compile).expect("open outer");
    ctx.call_list(inner).expect("record inner call");
    ctx.push_matrix().expect("record outer push");
    ctx.pop_matrix().expect("record outer pop");
    ctx.end_list().expect("end outer");

    assert_eq!(ctx.current_matrix_mode(), MatrixMode::ModelView);
    ctx.call_list(outer)
        .expect("replay must not underflow texture stack");
    assert_eq!(
        ctx.current_matrix_mode(),
        MatrixMode::Texture,
        "inner list switched the matrix mode; replay must leave it there"
    );
    // Зеркало глубины тоже должно быть texture-стеком (0 после push/pop).
    assert_eq!(
        ctx.pop_matrix(),
        Err(FbError::MatrixStackUnderflow { mode: "texture" })
    );
}

#[test]
fn replay_syncs_matrix_depth_for_later_live_calls() {
    let _guard = gl_test_lock();
    let mut fb = vec![0u32; 4 * 4];
    let ctx = fresh_context(&mut fb);

    let list = ctx.gen_lists(1).expect("list");
    ctx.new_list(list, ListMode::Compile).expect("open");
    ctx.matrix_mode(MatrixMode::Texture);
    for _ in 0..5 {
        ctx.push_matrix().expect("recorded push");
    }
    ctx.end_list().expect("end");
    ctx.call_list(list).expect("replay five pushes");
    // Texture-предел 8: после пяти занято 5, доступны ещё две.
    ctx.push_matrix().expect("sixth");
    ctx.push_matrix().expect("seventh");
    assert_eq!(
        ctx.push_matrix(),
        Err(FbError::MatrixStackOverflow { mode: "texture" }),
        "live mirror must know the five pushes recorded in the list"
    );
}
