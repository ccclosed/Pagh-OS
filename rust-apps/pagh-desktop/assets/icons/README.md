# assets/icons — готовые иконки Fluent UI System Icons

Монохромные regular-глифы [Fluent UI System Icons](https://github.com/microsoft/fluentui-system-icons)
(MIT, © Microsoft Corporation), отрендеренные в PNG 16/20/24/32 px. Каждый
файл — чёрный `#000000` + альфа; `src/icons.rs` заменяет RGB цветом темы,
поэтому один ассет работает и для обычного, и для акцентного, и для
приглушённого состояния. Filled-варианты не поставляются: они приходят как
`#212121` и на тёмной панели требуют той же перекраски.

| файл | Fluent-глиф | где используется |
|---|---|---|
| `grid-*.png` | Grid | панель, лаунчер |
| `terminal-*.png` | Window Console | панель, терминал |
| `folder-*.png` | Folder | панель, файлы |
| `image-*.png` | Image | панель, Paint |
| `network-wired-*.png` | Plug Connected | панель (20 px) и карточка флайаута «Сети» (24 px) — проводная сеть, не Wi-Fi |
| `window-minimize-*.png` | Subtract | кнопка окна `_` |
| `window-maximize-*.png` | Square | кнопка окна `□` |
| `window-close-*.png` | Dismiss | кнопка окна `×` |

Лицензия: [`LICENSE`](LICENSE) — MIT, Copyright (c) 2020 Microsoft Corporation.
Исходные SVG не входят в репозиторий: PNG подготовлены из официального
набора Fluent; соответствие имён-ключей — `manifest.json` в кэше подготовки
(`schema: pagh-icons/1`, `license: MIT`).
