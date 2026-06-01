# Extera V2T Server

Микросервисный сервер для преобразования аудиоконтента в текст на Rust. Использует **Whisper.cpp** для распознавания речи и **RabbitMQ** для асинхронной обработки задач.

## 🎯 Основные возможности

- **Асинхронная обработка** — задачи обрабатываются через очередь RabbitMQ
- **Поддержка множественных форматов** — OGG, MP3, WAV, WebM, M4A
- **Высокая производительность** — встроенное потоковое сохранение файлов (streaming)
- **Масштабируемость** — несколько воркеров могут обрабатывать задачи параллельно
- **Контроль размера файлов** — ограничение на максимальный размер загружаемого файла
- **Автоматическая очистка** — удаление старых результатов через TTL
- **Безопасность** — валидация имён файлов и защита от инъекций

## 📦 Технологический стек

- **Язык**: Rust 1.70+
- **Веб-фреймворк**: Axum 0.8
- **Message Queue**: RabbitMQ + amqprs
- **Обработка аудио**: FFmpeg + Whisper.cpp
- **Асинхронность**: Tokio
- **Логирование**: Tracing

## 🏗️ Архитектура

```
API Server (Axum)
      ↓
   [RabbitMQ Queue]
      ↓
Worker Process (FFmpeg + Whisper.cpp)
      ↓
Result JSON File
```

### Компоненты

- **`api/`** — REST API сервер для принятия файлов и выдачи результатов
- **`worker/`** — обработчик задач из очереди RabbitMQ
- **`shared/`** — общие типы данных (TranscriptionTask, TranscriptionResponse)

## 📋 Требования

### Обязательно

- **Rust** 1.70+ ([установка](https://rustup.rs/))
- **RabbitMQ** 3.8+ (или Docker контейнер)
- **FFmpeg** (для конвертации аудио)
- **Whisper.cpp** (`whisper-cli` в PATH)
- Модель Whisper.cpp (например, `ggml-small.bin`)

### Опционально

- **Docker** для контейнеризации
- **Docker Compose** для развёртывания всех сервисов

## 🚀 Запуск

### Локальная разработка

#### 1. Установка зависимостей

```bash
# RabbitMQ (если используется Docker)
docker run -d --name rabbitmq \
  -p 5672:5672 \
  -p 15672:15672 \
  rabbitmq:3-management

# FFmpeg
# Ubuntu/Debian:
sudo apt-get install ffmpeg

# macOS:
brew install ffmpeg

# Whisper.cpp
git clone https://github.com/ggerganov/whisper.cpp.git
cd whisper.cpp && make

# Скачать модель (например, small)
bash ./models/download-ggml-model.sh small
```

#### 2. Переменные окружения

Создайте файл `.env` в корне проекта:

```env
# RabbitMQ
RABBITMQ_HOST=localhost
RABBITMQ_USER=guest
RABBITMQ_PASS=guest

# API Server
HOST=127.0.0.1

# Log level
RUST_LOG=info
```

#### 3. Запуск API сервера

```bash
cd api
cargo run --release
```

Сервер запустится на `http://127.0.0.1:3000`

#### 4. Запуск воркера

В отдельном терминале:

```bash
cd worker
cargo run --release
```

## 📡 API Примеры

### Загрузить файл для транскрибации

```bash
curl -X POST http://127.0.0.1:3000/transcribe \
  -F "file=@audio.mp3" \
  -G \
  --data-urlencode "chat_id=user_123" \
  --data-urlencode "message_id=msg_456"
```

**Ответ:**
```json
{
  "task_id": "550e8400-e29b-41d4-a716-446655440000",
  "status": "queued",
  "file_path": "/tmp/transcribe/550e8400-e29b-41d4-a716-446655440000_audio.mp3"
}
```

### Получить результат

```bash
curl http://127.0.0.1:3000/result/550e8400-e29b-41d4-a716-446655440000
```

**Успешный ответ (когда транскрибация завершена):**
```json
{
  "text": "Привет, это пример распознавания речи"
}
```

**Ответ при обработке или не найдено:**
```json
{
  "status": "processing_or_not_found"
}
```

### Здоровье сервера

```bash
curl http://127.0.0.1:3000/health
```

**Ответ:**
```json
{
  "status": "ok",
  "service": "transcribe-api"
}
```

## ⚙️ Конфигурация

### API Server

| Переменная | Описание | По умолчанию |
|---|---|---|
| `RABBITMQ_HOST` | Хост RabbitMQ | `localhost` |
| `RABBITMQ_USER` | Пользователь RabbitMQ | `guest` |
| `RABBITMQ_PASS` | Пароль RabbitMQ | `guest` |
| `HOST` | IP адрес для привязки | `127.0.0.1` |

**Жёсткие параметры в коде:**
- Порт API: `3000`
- Максимальный размер файла: `500 MB`
- Максимальный размер загрузки: `500 MB`
- Директория временных файлов: `/tmp/transcribe`
- TTL результатов: `1 час` (удаляются автоматически)

### Worker

| Переменная | Описание | По умолчанию |
|---|---|---|
| `RABBITMQ_HOST` | Хост RabbitMQ | `localhost` |
| `RABBITMQ_USER` | Пользователь RabbitMQ | `guest` |
| `RABBITMQ_PASS` | Пароль RabbitMQ | `guest` |

**Жёсткие параметры:**
- Путь к модели Whisper: `/models/ggml-small.bin`
- Язык: `auto` (автоматическое определение)
- QoS Prefetch: `1` задача на воркер

## 🐳 Docker Compose

```yaml
version: '3.8'

services:
  rabbitmq:
    image: rabbitmq:3-management
    ports:
      - "5672:5672"
      - "15672:15672"
    environment:
      RABBITMQ_DEFAULT_USER: guest
      RABBITMQ_DEFAULT_PASS: guest
    volumes:
      - rabbitmq_data:/var/lib/rabbitmq

  api:
    build: ./api
    ports:
      - "3000:3000"
    environment:
      RABBITMQ_HOST: rabbitmq
      HOST: 0.0.0.0
      RUST_LOG: info
    depends_on:
      - rabbitmq
    volumes:
      - transcribe_data:/tmp/transcribe

  worker:
    build: ./worker
    environment:
      RABBITMQ_HOST: rabbitmq
      RUST_LOG: info
    depends_on:
      - rabbitmq
    volumes:
      - transcribe_data:/tmp/transcribe
      - ./models:/models

volumes:
  rabbitmq_data:
  transcribe_data:
```

Запуск:
```bash
docker-compose up --build
```

## 📊 Поток обработки

1. **Клиент** отправляет файл на `/transcribe` с параметрами `chat_id` и `message_id`
2. **API Server**:
   - Валидирует файл (размер, формат)
   - Сохраняет файл потоком в `/tmp/transcribe`
   - Создаёт задачу `TranscriptionTask`
   - Отправляет задачу в RabbitMQ очередь
   - Возвращает `task_id`
3. **Worker** получает задачу:
   - Конвертирует аудио в WAV 16kHz mono через FFmpeg
   - Запускает Whisper.cpp на файле
   - Извлекает текст из JSON результата
   - Сохраняет результат в `/tmp/transcribe/{task_id}.json`
   - Удаляет временные файлы
   - Подтверждает (ACK) задачу в RabbitMQ
4. **Клиент** получает результат, опрашивая `/result/{task_id}`
5. **Cleanup Task** удаляет результаты старше 1 часа

## 🔧 Разработка

### Структура проекта

```
.
├── api/                          # REST API сервер
│   ├── src/
│   │   └── main.rs
│   └── Cargo.toml
├── worker/                       # Обработчик задач
│   ├── src/
│   │   └── main.rs
│   └── Cargo.toml
├── shared/                       # Общие типы
│   ├── src/
│   │   └── lib.rs
│   └── Cargo.toml
├── Cargo.toml                    # Workspace manifest
└── README.md
```

### Сборка

```bash
# Всё сразу
cargo build --release

# Конкретный модуль
cargo build --release -p api
cargo build --release -p worker
```

### Тесты

```bash
cargo test
RUST_LOG=debug cargo test -- --nocapture
```

## 🐛 Известные ограничения

- Хранилище результатов — файловая система (`/tmp/transcribe`), не масштабируется на несколько машин
- Нет базы данных для статусов задач — может быть неточностью при отправке статуса
- Модель Whisper жёстко кодирована на `small` — требуется изменение исходного кода для другой модели
- Результаты удаляются через 1 час — нет постоянного хранилища

## 🚀 Возможные улучшения

- [ ] Redis для кэширования статусов задач
- [ ] PostgreSQL для хранения метаданных
- [ ] WebSocket для real-time статуса
- [ ] S3/MinIO для хранения файлов
- [ ] Настройка модели Whisper через переменные окружения
- [ ] Поддержка webhook callbacks
- [ ] Метрики Prometheus
- [ ] Graceful shutdown и drain очереди

## 📝 Лицензия

MIT

## 💬 Поддержка

Вопросы и предложения:
- [Issues](https://github.com/shizamuru-dev/Extera-V2T-Server/issues)
- [Discussions](https://github.com/shizamuru-dev/Extera-V2T-Server/discussions)

---

**Разработано на Rust** 🦀
