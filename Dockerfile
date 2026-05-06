FROM rust:1.85-slim AS builder

WORKDIR /usr/src/app

# Устанавливаем зависимости для сборки whisper.cpp
RUN apt-get update && apt-get install -y git make g++ wget cmake

# Клонируем и собираем whisper.cpp
RUN git clone https://github.com/ggerganov/whisper.cpp.git /whisper && \
    cd /whisper && \
    cmake -B build -DBUILD_SHARED_LIBS=OFF && cmake --build build --config Release && \
    bash ./models/download-ggml-model.sh small

# Скопируем манифесты рабочих пространств
COPY Cargo.toml Cargo.lock ./
COPY shared/Cargo.toml shared/
COPY api/Cargo.toml api/
COPY worker/Cargo.toml worker/

# Создадим заглушки для кеширования зависимостей
RUN mkdir -p shared/src api/src worker/src && \
    touch shared/src/lib.rs api/src/main.rs worker/src/main.rs

RUN cargo fetch

# Теперь копируем исходный код
COPY shared/src ./shared/src
COPY api/src ./api/src
COPY worker/src ./worker/src

# Собираем бинарники Rust
RUN cargo build --release --bin api
RUN cargo build --release --bin worker

# Подготавливаем библиотеки whisper (на случай если они динамические)
RUN mkdir -p /whisper_libs && \
    cp /whisper/build/src/libwhisper.so* /whisper_libs/ 2>/dev/null || true

# Финальный образ
FROM debian:bookworm-slim
# Устанавливаем ffmpeg для конвертации аудио и ca-certificates
RUN apt-get update && \
    apt-get install -y ca-certificates ffmpeg && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Копируем собранные бинарники Rust
COPY --from=builder /usr/src/app/target/release/api /usr/local/bin/api
COPY --from=builder /usr/src/app/target/release/worker /usr/local/bin/worker

# Копируем whisper.cpp и модель
COPY --from=builder /whisper/build/bin/whisper-cli /usr/local/bin/whisper-cli
COPY --from=builder /whisper_libs/ /usr/local/lib/
RUN ldconfig

COPY --from=builder /whisper/models/ggml-small.bin /models/ggml-small.bin

# Директория для временных файлов
RUN mkdir -p /tmp/transcribe && chmod 777 /tmp/transcribe

EXPOSE 3000
