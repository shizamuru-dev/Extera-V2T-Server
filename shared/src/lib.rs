use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionTask {
    /// Уникальный ID задачи
    pub task_id: String,
    /// Полный путь до файла на сервере
    pub file_path: String,
    /// ID чата в Telegram
    pub chat_id: String,
    /// ID сообщения в Telegram (для редактирования)
    pub message_id: Option<String>,
    /// Имя файла (для логирования)
    pub filename: String,
    /// Таймстамп создания
    pub created_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TranscriptionResponse {
    pub task_id: String,
    pub status: String,
    pub file_path: String,
}
