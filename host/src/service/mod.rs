//! Сервисный слой: потоки-исполнители доменных ядер (эффекты, IO,
//! трансляция событий). Ядра — чистые (model/), железо — driver/.

pub mod key_matcher;
pub mod race;
pub mod transport;
