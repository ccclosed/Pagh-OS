# assets/icons — готовые иконки Fluent UI System Icons

Монохромные regular-глифы [Fluent UI System Icons](https://github.com/microsoft/fluentui-system-icons)
(MIT, © Microsoft Corporation), отрендеренные в PNG 16/20/24/32 px. Каждый
файл — чёрный `#000000` + альфа; `src/icons.rs` заменяет RGB цветом темы,
поэтому один ассет работает и для обычного, и для акцентного, и для
приглушённого состояния. Filled-варианты не поставляются: они приходят как
`#212121` и на тёмном фоне требуют той же перекраски.

| файл | Fluent-глиф | где используется |
|---|---|---|
| `folder-*.png` | Folder | записи-каталоги (сетка и список) |
| `folder-open-*.png` | Folder Open | пункт сайдбара «Документы» |
| `document-*.png` | Document Text | файлы без известного расширения, символические ссылки |
| `home-*.png` | Home | сайдбар, «Домашняя папка» |
| `desktop-*.png` | Desktop | сайдбар, «Рабочий стол» |
| `arrow-download-*.png` | Arrow Download | сайдбар, «Загрузки» |
| `image-*.png` | Image | файлы `png/jpg/gif/…`, сайдбар «Изображения» |
| `music-note-*.png` | Music Note 2 | файлы `mp3/flac/ogg/…`, сайдбар «Музыка» |
| `video-*.png` | Movies and TV | файлы `mp4/mkv/…`, сайдбар «Видео» |
| `delete-*.png` | Delete | сайдбар, «Корзина» |
| `drive-*.png` | Hard Drive | сайдбар, точки монтирования; записи-устройства |
| `network-*.png` | Globe | сайдбар, «Сеть» |

Лицензия: [`LICENSE`](LICENSE) — MIT, Copyright (c) 2020 Microsoft Corporation.
Исходные SVG в репозиторий не входят: PNG подготовлены из официального набора
Fluent; соответствие имён-ключей — `manifest.json` кэша подготовки
(`schema: pagh-icons/1`, `license: MIT`).

Декодер PNG/inflate — собственный, без внешних зависимостей: `src/png.rs` и
`src/deflate.rs` (повторяют минимальную часть `rust-apps/pagh-desktop`).