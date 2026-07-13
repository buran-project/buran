# 🚀 Buran Application Server

[English](README.md) · **Русский** · [中文](README.zh.md)

[![Project Status: Active](https://www.repostatus.org/badges/latest/active.svg)](https://www.repostatus.org/#active)
[![Tests](https://github.com/buran-project/buran/actions/workflows/tests.yml/badge.svg)](https://github.com/buran-project/buran/actions/workflows/tests.yml)
[![Security Scan](https://github.com/buran-project/buran/actions/workflows/scan.yml/badge.svg)](https://github.com/buran-project/buran/actions/workflows/scan.yml)
[![Release](https://github.com/buran-project/buran/actions/workflows/release.yml/badge.svg)](https://github.com/buran-project/buran/actions/workflows/release.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-CE422B?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Container](https://img.shields.io/badge/ghcr.io-buran--project%2Fburan-2496ED?logo=docker&logoColor=white)](https://github.com/buran-project/buran/pkgs/container/buran)
[![License](https://img.shields.io/github/license/buran-project/buran)](LICENSE)

**Buran** — сервер приложений, написанный на Rust. Он отдаёт статику,
маршрутизирует запросы и исполняет код вашего приложения в пуле встроенных
рабочих процессов — весь фронтир описывается одним декларативным YAML-конфигом.

Ядро не привязано к языку: рантаймы подключаются отдельными модулями, а не
вкомпилированы в сервер. **PHP — первый поддерживаемый рантайм**, дальше будут
ещё.

Если вы работали с **nginx + FastCGI** или **NGINX Unit**, схема знакомая:
сеть и маршрутизация в одном процессе, воркеры исполняют код приложения, и
никакого отдельного демона между ними.

## ✨ Зачем Buran?

- 📦 **Один конфиг, один фронтир** — роутер, отдача статики и супервизор
  процессов живут в одном нативном серверном бинаре; слушатели, маршруты и
  приложения — всё описано в одном YAML-файле.
- 🔌 **Подключаемые рантаймы** — рантаймы подключаются отдельными модульными
  бинарями `buran-<runtime>`, поэтому несколько версий работают рядом
  (`buran-php83`, `buran-php84`, …), а новые языки добавляются без правки ядра.
- ⚡ **Исполнение в процессе** — рабочие процессы исполняют код приложения прямо
  под управлением сервера — без FastCGI-прыжка и без внешнего менеджера
  процессов.
- 🛡️ **Безопасно по умолчанию** — исходники, которые рантайм объявляет
  исполняемыми, **никогда** не отдаются как статика, даже если под них подходит
  какое-то правило.
- 🧊 **Статическая конфигурация** — никакого live admin API. Изменение конфига
  означает reload/restart (или, в контейнерах, новый контейнер) — состояние
  остаётся предсказуемым и проверяемым.
- 🐳 **Родной для контейнеров** — корректно работает как PID 1 (подбирает
  осиротевшие процессы, аккуратно завершается по `SIGTERM`/`SIGINT`).
  Официальные образы рантаймов выходят на каждую версию.

## 🧩 Поддерживаемые языки

Рантаймы подключаются отдельными модулями, поэтому список растёт без правки
ядра. Что работает сегодня:

| Язык | Флейворы образов | Статус |
|------|------------------|--------|
| [![PHP](https://img.shields.io/badge/PHP-7.3_–_8.5-777BB4?logo=php&logoColor=white)](https://www.php.net/) | Debian, Alpine | ✅ Поддерживается |

Несколько версий сосуществуют рядом (`buran-php83`, `buran-php84`, …), так что
один образ обслуживает приложения, привязанные к разным веткам.

## 📋 Требования

Выберите путь:

- **Docker** — ничего, кроме среды запуска контейнеров. Официальные образы уже
  содержат Buran, PHP-модуль и opcache.
- **Из исходников** — Rust **1.85+**. PHP-модулю дополнительно нужен `libphp`,
  собранный с `--enable-embed`, плюс `php-config` и C-тулчейн (у самого ядра
  зависимости от PHP **нет**).

## ⚡ Быстрый старт

### 1. Напишите конфиг

Создайте `buran.yaml` — отдавать статику, с фолбэком на PHP-фронт-контроллер:

```yaml
settings:
  modules: /usr/lib/buran/modules   # куда образы ставят модули рантаймов

listeners:
  "*:8080":
    route: main

routes:
  main:
    - action:
        share: /www$uri            # сначала пробуем реальный файл
        fallback:
          application: app         # всё остальное → PHP

applications:
  app:
    module: php85                  # → /usr/lib/buran/modules/buran-php85
    root: /www
    index: index.php
    processes: 2
```

### 2. Запустите 🐳

```bash
docker run --rm -p 8080:8080 \
  -v "$PWD/buran.yaml:/etc/buran/buran.yaml:ro" \
  -v "$PWD/public:/www:ro" \
  ghcr.io/buran-project/buran:php
```

Откройте <http://localhost:8080>. Готово. 🎉

Не уверены, какое имя модуля указать? Посмотрите, что несёт образ:

```bash
docker run --rm ghcr.io/buran-project/buran:php buran --modules
```

### 3. …или соберите из исходников 🦀

```bash
git clone https://github.com/buran-project/buran
cd buran

# Ядро сервера (PHP-тулчейн не нужен):
cargo run -p buran -- --config examples/buran.yaml
```

### 4. Проверьте перед деплоем ✅

```bash
buran --check-config --config /etc/buran/buran.yaml
```

Проверяет схему **и** прощупывает каждый модуль рантайма на совместимость по
протоколу, затем выходит с ненулевым кодом при любой проблеме — можно смело
вешать как гейт перед деплоем.

## 🧭 Как это устроено

Запрос проходит через четыре вида конфиг-объектов:

```
   TCP :8080  ─►  listener  ─►  route  ─►  action  ─►  application
                (bind addr)   (match →    (share /     (модуль рантайма
                              action)     return /      + пул воркеров)
                                          app / route)
```

- **listener** привязывается к `host:port` и входит в маршрут (или отдаёт
  встроенный status-эндпоинт).
- **route** — упорядоченный список шагов `match → action`; побеждает первое
  совпадение.
- **action** отдаёт статику (`share`), возвращает код (`return`), прыгает в
  другой маршрут (`route`) или диспетчеризует в **application**.
- **application** связывает **модуль** рантайма с корнем документов и пулом
  рабочих процессов.

## 📚 Документация

Полная документация — в [`docs/`](docs/):

| Документ | Что внутри |
|----------|------------|
| 📖 [Getting started](docs/getting-started.md) | Запуск Buran через Docker или из исходников за пару минут. |
| ⚙️ [Configuration reference](docs/configuration.md) | `settings`, `listeners`, `access_log`, подстановка env, status-эндпоинт. |
| 🧭 [Routing](docs/routing.md) | Маршруты, условия `match`, действия, синтаксис паттернов, rewrite'ы. |
| 📁 [Static files](docs/static-files.md) | Действие `share`, index-файлы, MIME-типы, защита от утечки исходников. |
| 🧩 [Applications & runtimes](docs/applications.md) | Пулы процессов, лимиты, PHP-модуль, как устроены модули (BWP). |
| 🚢 [Deployment](docs/deployment.md) | Docker-образы, дистрибутивные пакеты, systemd, запуск как PID 1. |
| 🖥️ [CLI reference](docs/cli.md) | Флаги командной строки, переменные окружения, сигналы. |

## 🐳 Образы контейнеров

Официальные образы публикуются в GitHub Container Registry:

| Образ | Содержимое |
|-------|------------|
| `ghcr.io/buran-project/buran:php` | Buran + последний PHP-модуль рантайма + opcache. |
| `ghcr.io/buran-project/buran:php-alpine` | То же, флейвор Alpine. |
| `ghcr.io/buran-project/buran:minimal` | Только Buran — статика и маршрутизация, без модуля рантайма. |

Ветки PHP **7.3 → 8.5** собираются матрицей, во флейворах Debian и Alpine.
Готовые **дистрибутивные пакеты** (Alpine / Debian / RPM) прикладываются к
каждому [релизу](https://github.com/buran-project/buran/releases). Полный список
и теги — в [Deployment](docs/deployment.md).

## 🛠️ Разработка

Buran — это Rust-воркспейс:

| Крейт | Роль |
|-------|------|
| `buran` | Главный процесс: CLI, загрузка конфига, проверка модулей, супервизия. |
| `buran-router` | HTTP/1.1, маршрутизация, rewrite'ы, статика, диспетчеризация, WebSocket. |
| `buran-config` | Схема конфига, валидация, подстановка `${ENV}`. |
| `buran-ipc` | Buran Worker Protocol (BWP): фрейминг и плоское кодирование запросов. |
| `buran-worker` | SDK на стороне воркера для сборки модулей рантаймов. |
| `buran-php` | PHP-модуль рантайма: встроенный `libphp` через кастомный SAPI. |
| `buran-echo` | Референсный event-loop модуль (конкурентный профиль BWP). |

```bash
cargo build            # собрать воркспейс
cargo test             # прогнать тесты
cargo run -p buran -- --config examples/buran.yaml
```

## 📄 Лицензия

Распространяется под [Apache License 2.0](LICENSE).

---

**Репозиторий**: [github.com/buran-project/buran](https://github.com/buran-project/buran)
**Реестр контейнеров**: [ghcr.io/buran-project/buran](https://github.com/buran-project/buran/pkgs/container/buran)

♥️ Issue'ы и pull request'ы приветствуются!
