// Package config управляет конфигурацией приложения
package config

import (
	"cmp"
	"fmt"
	"os"
	"path/filepath"

	"gopkg.in/yaml.v3"

	"github.com/fpvladder/laps/host/internal/locale"
)

// Config структура конфигурации
type Config struct {
	Locale locale.Locale `yaml:"locale,omitempty"` // язык интерфейса, например ru, en-US, pt-BR
}

// Default возвращает конфигурацию по умолчанию
func Default() *Config {
	return &Config{
		Locale: locale.System(),
	}
}

// Load загружает конфигурацию из файла.
// Если файл не существует — возвращает пустой конфиг.
func Load(providedConfigPath string) (*Config, error) {
	configPath := providedConfigPath
	if configPath == "" {
		defaultPath, err := getConfigPath()
		if err != nil {
			return nil, fmt.Errorf("не удалось определить путь к конфигу: %w", err)
		}
		configPath = defaultPath
	}

	// Если файл не существует — возвращаем пустой конфиг
	if _, err := os.Stat(configPath); os.IsNotExist(err) {
		return &Config{}, nil
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

	return &cfg, nil
}

// Override возвращает копию текущего конфига, в которой непустые поля other
// заменяют соответствующие поля.
func (c *Config) Override(other *Config) *Config {
	if other == nil {
		return c
	}
	return &Config{
		Locale: cmp.Or(other.Locale, c.Locale),
	}
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
