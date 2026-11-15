<!--
Спасибо за PR! Заполните разделы ниже; для крупных изменений сначала issue.
См. CONTRIBUTING.md — процесс, ветки и релизы.
-->

## Что меняется и зачем

<!-- Кратко: проблема, решение, затронутые компоненты. -->

## Связанный issue

<!-- Closes #123 / Refs #123 -->

## Как проверено

- [ ] `cargo fmt --all -- --check`
- [ ] `python3 tools/build.py build` (debug + link)
- [ ] `python3 tools/build.py build --release` (release + link)
- [ ] `python3 tools/host_tests.py`
- [ ] `python3 tools/check_safety.py`
- [ ] `python3 tools/check_agents_md.py`
- [ ] in-QEMU `selftest` без `FAIL` (если менялось ядро)
- [ ] `tools/e2e.py <mode>` для затронутого сценария (если применимо)
- [ ] ручной прогон / реальное железо (если применимо)

<!-- Для изменений поведения: before/after serial, скриншот framebuffer, e2e-вывод. -->

## Риски и совместимость

<!-- Ломает ли API/ABI/формат данных? Нужна ли миграция? Влияние на security-модель? -->

## Чек-лист

- [ ] PR направлен в `dev` (hotfix в `main` — только critical/high verified security или release-blocking regression).
- [ ] Build invariants сохранены (`#![no_std]`, `panic = abort`, custom target, higher-half, frame pointers, host-tests isolation).
- [ ] Новый `unsafe` имеет `// SAFETY:` и проходит `check_safety`.
- [ ] Документация/комментарии обновлены, если изменилось поведение.
- [ ] Нет случайных файлов (`target/`, `iso_root/`, `disk.img`, `PAGH.elf`, логи, IDE).
- [ ] Нет секретов/токенов/приватных ключей в диффе.
