// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! Учёт display lists для безопасного слоя [`crate::Context`].
//!
//! Upstream `list.c` — это интерпретатор записанных опкодов: `glNewList`
//! включает `compile_flag`, `gl_add_op` пишет каждую операцию в буфер, а
//! `glCallList` проигрывает буфер напрямую через `op_table_func`. У этой схемы
//! три опасных для safe-обёртки свойства:
//!
//! * `shared_state.lists[list]` индексируется **без проверки границ**
//!   (`MAX_DISPLAY_LISTS == 1024`), а `glCallList` на неизвестном имени делает
//!   `gl_fatal_error` → `exit(1)`;
//! * `glPushMatrix`/`glPopMatrix` внутри списка на реплее упираются в
//!   C-`assert`, если глубины не сходятся с моментом вызова;
//! * если в списке включён `GL_TEXTURE_2D`, а текущая текстура не имеет
//!   картинки, реплей заливного треугольника разыменовывает `NULL`
//!   (`clip.c: ZB_setTexture(c->zb, c->current_texture->images[0].pixmap)`).
//!
//! `glDeleteLists` в 0.4.1 — пустой `inline` в `gl.h`; внешнего символа нет,
//! и C-слот списка не освобождается никогда. Поэтому модуль ведёт **зеркало
//! занятости слотов** (множество когда-либо выданных имён) — это точный образ
//! `shared_state.lists`, ведь слоты создаются только через `glGenLists`/
//! `glNewList`, а удаления в C нет. По этому зеркалу мы предсказываем имя,
//! которое вернёт `glGenLists`, и отличаем успех от отказа (upstream возвращает
//! `0` в обоих случаях, если диапазон начинается с нуля).
//!
//! Модель реплея [`simulate`] проверяет ровно те условия, на которых C упал бы
//! (`assert`/`fatal`/`NULL`), **до** вызова `glCallList`: глубины матричных
//! стеков по режимам, вложенность `glBegin`, валидность текстуры в момент
//! текстурного треугольника и отсутствие рекурсии списков. Всё, что не влияет
//! на падения (цвета, координаты, матричные значения), не моделируется.

use std::collections::{BTreeMap, BTreeSet};

use crate::ffi::MAX_DISPLAY_LISTS;
use crate::{FbError, MatrixMode, Primitive};

/// Режим `glNewList` (`GL_COMPILE` / `GL_COMPILE_AND_EXECUTE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMode {
    /// Только запись; операции не исполняются до `glCallList`.
    Compile,
    /// Запись + немедленное исполнение.
    CompileAndExecute,
}

impl ListMode {
    pub(crate) fn to_gl(self) -> i32 {
        match self {
            ListMode::Compile => crate::ffi::GL_COMPILE,
            ListMode::CompileAndExecute => crate::ffi::GL_COMPILE_AND_EXECUTE,
        }
    }
}

/// Открытая компиляция: `glNewList` был, `glEndList` ещё нет.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompileState {
    pub(crate) id: u32,
    pub(crate) mode: ListMode,
}

/// Операция, которая влияет на состояние реплея и потому записывается в
/// зеркало списка. Геометрия пишется как [`ListOp::Vertex`] без координат:
/// для проверок опасных условий нужен только факт вершины.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListOp {
    Begin(Primitive),
    End,
    Vertex,
    PushMatrix,
    PopMatrix,
    MatrixMode(MatrixMode),
    BindTexture(u32),
    EnableTexture,
    DisableTexture,
    CallList(u32),
}

/// Что записано в список и не «отравлен» ли он.
#[derive(Debug, Default, Clone)]
pub(crate) struct ListMeta {
    pub(crate) ops: Vec<ListOp>,
    /// В список попало то, что на реплее гарантированно упало бы (вершина вне
    /// `glBegin`, несбалансированный `begin`): список остаётся определённым,
    /// но `call_list` его отвергает.
    pub(crate) poisoned: bool,
}

/// Зеркало list-слотов C-контекста.
#[derive(Debug, Default)]
pub(crate) struct ListRegistry {
    /// Имена, когда-либо выданные `glGenLists`. C-слоты не освобождаются,
    /// поэтому множество — точная карта занятости `shared_state.lists`.
    generated: BTreeSet<u32>,
    /// Имена, определённые для вызова прямо сейчас. `delete_lists` убирает
    /// имя отсюда (эмуляция GL), но не из `generated`.
    defined: BTreeMap<u32, ListMeta>,
}

impl ListRegistry {
    pub(crate) fn is_defined(&self, id: u32) -> bool {
        self.defined.contains_key(&id)
    }

    pub(crate) fn is_generated(&self, id: u32) -> bool {
        self.generated.contains(&id)
    }

    /// Первый свободный непрерывный диапазон длины `range` — ровно то имя,
    /// которое вернёт upstream `glGenLists` (включая `0`).
    pub(crate) fn first_free_run(&self, range: u32) -> Option<u32> {
        if range == 0 || range > MAX_DISPLAY_LISTS {
            return None;
        }
        let mut start = 0;
        let mut count = 0;
        for id in 0..MAX_DISPLAY_LISTS {
            if self.generated.contains(&id) {
                count = 0;
                start = id + 1;
            } else {
                count += 1;
                if count == range {
                    return Some(start);
                }
            }
        }
        None
    }

    /// Записать диапазон, выданный C. Каждое имя сразу определено пустым
    /// списком — ровно как `alloc_list` в `glGenLists`.
    pub(crate) fn insert_generated(&mut self, first: u32, range: u32) {
        for id in first..first + range {
            self.generated.insert(id);
            self.defined.entry(id).or_default();
        }
    }

    /// Начать компиляцию имени (пере)определением: старые операции
    /// отбрасываются — так же `glNewList` удаляет прежний список.
    pub(crate) fn open(&mut self, id: u32) {
        self.defined.insert(id, ListMeta::default());
    }

    pub(crate) fn record(&mut self, id: u32, op: ListOp) {
        if let Some(meta) = self.defined.get_mut(&id) {
            meta.ops.push(op);
        }
    }

    pub(crate) fn poison(&mut self, id: u32) {
        if let Some(meta) = self.defined.get_mut(&id) {
            meta.poisoned = true;
        }
    }

    /// Завершить компиляцию; возвращает `true`, если список отравлен.
    pub(crate) fn finish(&mut self, id: u32) -> bool {
        self.defined.get(&id).is_some_and(|meta| meta.poisoned)
    }

    /// Эмуляция `glDeleteLists`: имя перестаёт быть определённым, но слот в C
    /// (и в `generated`) остаётся занятым навсегда.
    pub(crate) fn forget(&mut self, id: u32) {
        self.defined.remove(&id);
    }

    pub(crate) fn meta(&self, id: u32) -> Option<&ListMeta> {
        self.defined.get(&id)
    }
}

/// Состояние, которое переживает реплей списка и проверяется по ходу.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SimState {
    /// Глубина push поверх базы для `[ModelView, Projection, Texture]`.
    pub(crate) depth: [usize; 3],
    pub(crate) mode: MatrixMode,
    pub(crate) texture_enabled: bool,
    pub(crate) bound_texture: Option<u32>,
    pub(crate) in_begin: bool,
    pub(crate) begin_primitive: Option<Primitive>,
}

impl SimState {
    fn require_texture(&self, textured: &BTreeSet<u32>) -> Result<(), FbError> {
        match self.bound_texture {
            None => Err(FbError::TextureNotBound),
            Some(texture) if !textured.contains(&texture) => {
                Err(FbError::TextureHasNoImage { texture })
            }
            Some(_) => Ok(()),
        }
    }
}

/// Проиграть список `id` на зеркале состояния и убедиться, что C-реплей не
/// упрётся в `assert`/`fatal`/`NULL`. `created` пополняется именами текстур,
/// которые C создаст по ходу `glBindTexture`.
///
/// `call_stack` — путь вызовов для обнаружения рекурсии: upstream на ней
/// переполняет C-стек, мы возвращаем [`FbError::ListCycle`].
pub(crate) fn simulate(
    registry: &ListRegistry,
    id: u32,
    state: &mut SimState,
    limits: &[usize; 3],
    textured: &BTreeSet<u32>,
    created: &mut Vec<u32>,
    call_stack: &mut Vec<u32>,
) -> Result<(), FbError> {
    if call_stack.contains(&id) {
        return Err(FbError::ListCycle { list: id });
    }
    let Some(meta) = registry.meta(id) else {
        return Err(FbError::ListUndefined { list: id });
    };
    if meta.poisoned {
        return Err(FbError::ListMalformed { list: id });
    }

    let entry_in_begin = state.in_begin;
    let entry_primitive = state.begin_primitive;
    call_stack.push(id);

    for op in &meta.ops {
        match *op {
            ListOp::Begin(primitive) => {
                if state.in_begin {
                    return Err(FbError::BeginNested);
                }
                state.in_begin = true;
                state.begin_primitive = Some(primitive);
            }
            ListOp::End => {
                if !state.in_begin {
                    return Err(FbError::EndWithoutBegin);
                }
                let textured_draw = state
                    .begin_primitive
                    .is_some_and(Primitive::rasterizes_triangles)
                    && state.texture_enabled;
                if textured_draw {
                    state.require_texture(textured)?;
                }
                state.in_begin = false;
                state.begin_primitive = None;
            }
            ListOp::Vertex => {
                if !state.in_begin {
                    return Err(FbError::ListMalformed { list: id });
                }
                // Вершина, дорисовывающая треугольник/квад, уходит в
                // `ZB_fillTriangleMappingPerspective`, который разыменовывает
                // `current_texture->images[0].pixmap`.
                let textured_draw = state
                    .begin_primitive
                    .is_some_and(Primitive::rasterizes_triangles)
                    && state.texture_enabled;
                if textured_draw {
                    state.require_texture(textured)?;
                }
            }
            ListOp::PushMatrix => {
                // Режим берётся на момент реплея: вложенный `CallList` или
                // записанный `MatrixMode` могли его сменить.
                let mode = state.mode;
                let index = mode.index();
                if state.depth[index] + 1 >= limits[index] {
                    return Err(FbError::MatrixStackOverflow { mode: mode.name() });
                }
                state.depth[index] += 1;
            }
            ListOp::PopMatrix => {
                let index = state.mode.index();
                if state.depth[index] == 0 {
                    return Err(FbError::MatrixStackUnderflow {
                        mode: state.mode.name(),
                    });
                }
                state.depth[index] -= 1;
            }
            ListOp::MatrixMode(mode) => state.mode = mode,
            ListOp::BindTexture(texture) => {
                state.bound_texture = Some(texture);
                created.push(texture);
            }
            ListOp::EnableTexture => {
                state.require_texture(textured)?;
                state.texture_enabled = true;
            }
            ListOp::DisableTexture => state.texture_enabled = false,
            ListOp::CallList(nested) => {
                simulate(
                    registry, nested, state, limits, textured, created, call_stack,
                )?;
            }
        }
    }

    call_stack.pop();
    // Список обязан вернуть `glBegin`-состояние в то, в котором его вызвали:
    // `end_list` не даёт сохранить открытый begin, но вложенный вызов внутри
    // чужого begin не должен его закрывать.
    if state.in_begin != entry_in_begin || state.begin_primitive != entry_primitive {
        return Err(FbError::ListMalformed { list: id });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_run_matches_upstream_scan() {
        let mut registry = ListRegistry::default();
        assert_eq!(registry.first_free_run(1), Some(0));
        assert_eq!(registry.first_free_run(0), None);
        assert_eq!(registry.first_free_run(MAX_DISPLAY_LISTS + 1), None);

        registry.insert_generated(0, 3);
        assert_eq!(registry.first_free_run(1), Some(3));
        assert_eq!(registry.first_free_run(2), Some(3));

        registry.insert_generated(3, 1);
        // 0..=3 заняты, следующий свободный — 4.
        assert_eq!(registry.first_free_run(1), Some(4));

        registry.insert_generated(4, 1);
        assert_eq!(registry.first_free_run(1), Some(5));

        registry.insert_generated(5, MAX_DISPLAY_LISTS - 5);
        assert_eq!(registry.first_free_run(1), None, "all 1024 slots occupied");
    }

    #[test]
    fn delete_only_hides_the_name() {
        let mut registry = ListRegistry::default();
        registry.insert_generated(2, 2); // 2, 3
        assert!(registry.is_defined(2));
        registry.forget(2);
        assert!(!registry.is_defined(2));
        assert!(registry.is_generated(2));
        assert!(!registry.is_generated(1));
        assert_eq!(
            registry.first_free_run(1),
            Some(0),
            "slot 2 stays occupied in C"
        );
    }
}
