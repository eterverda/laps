// Package config управляет конфигурацией приложения
package config

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"

	"golang.org/x/text/language"
	"gopkg.in/yaml.v3"
)

// Config структура конфигурации
type Config struct {
	Lang language.Tag `yaml:"lang"` // язык интерфейса, например ru, en-US, pt-BR
}

// Default возвращает конфигурацию по умолчанию
func Default() *Config {
	return &Config{
		Lang: detectSystemLang(),
	}
}

// Load загружает конфигурацию из файла или создаёт по умолчанию
func Load(providedConfigPath string) (*Config, error) {
	configPath := providedConfigPath
	if configPath == "" {
		defaultPath, err := getConfigPath()
		if err != nil {
			return nil, fmt.Errorf("не удалось определить путь к конфигу: %w", err)
		}
		configPath = defaultPath
	}

	// Если файл не существует — возвращаем дефолт
	if _, err := os.Stat(configPath); os.IsNotExist(err) {
		return Default(), nil
	}

	// Читаем файл
	data, err := os.ReadFile(configPath)
	if err != nil {
		return nil, fmt.Errorf("не удалось прочитать конфиг: %w", err)
	}

	var cfg Config
	if err := yaml.Unmarshal(data, &cfg); err != nil {
		return nil, fmt.Errorf("не удалось распарсить конфиг: %w", err)
	}

	// Если lang пустой — используем системный
	if cfg.Lang == language.Und {
		cfg.Lang = detectSystemLang()
	}

	return &cfg, nil
}

// getConfigPath возвращает путь к файлу конфигурации
func getConfigPath() (string, error) {
	// XDG_CONFIG_HOME или ~/.config
	var configDir string
	if xdgConfig := os.Getenv("XDG_CONFIG_HOME"); xdgConfig != "" {
		configDir = filepath.Join(xdgConfig, "laps")
	} else {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		configDir = filepath.Join(home, ".config", "laps")
	}

	return filepath.Join(configDir, "config.yaml"), nil
}

// detectSystemLang определяет язык системы
func detectSystemLang() language.Tag {
	lang := os.Getenv("LANG")
	if lang == "" {
		lang = os.Getenv("LC_ALL")
	}

	// ru_RU.UTF-8 → ru_RU
	if idx := strings.Index(lang, "."); idx != -1 {
		lang = lang[:idx]
	}

	// Если не определён — пробуем macOS
	if lang == "" || lang == "C" || lang == "POSIX" {
		if runtime.GOOS == "darwin" {
			// Пробуем defaults read
			// Здесь можно добавить exec.Command, но это замедлит старт
			// Пока просто en
		}
		return language.English
	}

	tag, err := language.Parse(lang)
	if err != nil {
		return language.English
	}
	return tag
}
