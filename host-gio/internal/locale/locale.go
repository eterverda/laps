// Package locale предоставляет тип Locale — обёртку над language.Tag
// с поддержкой сериализации YAML.
package locale

import (
	"fmt"
	"os"
	"runtime"
	"strings"

	"golang.org/x/text/language"
	"gopkg.in/yaml.v3"
)

// Locale — языковой тег с поддержкой YAML.
// Прозрачный алиас для language.Tag: можно использовать
// везде, где ожидается language.Tag, и наоборот.
type Locale language.Tag

// Предопределённые локали.
var (
	Russian = Locale(language.Russian)
	English = Locale(language.English)
)

// ── Методы типа ──────────────────────────────────────────────

// System определяет язык системы из переменных окружения LANG/LC_ALL.
// Поддерживает POSIX-форматы вида ru_RU.UTF-8.
func System() Locale {
	s := os.Getenv("LANG")
	if s == "" {
		s = os.Getenv("LC_ALL")
	}

	// Нормализуем системные форматы: ru_RU.UTF-8 → ru_RU
	if idx := strings.Index(s, "."); idx != -1 {
		s = s[:idx]
	}

	if s == "" || s == "C" || s == "POSIX" {
		if runtime.GOOS == "darwin" {
			// Пробуем defaults read
			// Здесь можно добавить exec.Command, но это замедлит старт
			// Пока просто en
		}
		return Locale(language.English)
	}

	loc, err := Parse(s)
	if err != nil {
		return Locale(language.English)
	}
	return loc
}

// String возвращает строковое представление тега.
// Пустая строка означает неопределённый язык (language.Und).
func (l Locale) String() string {
	if language.Tag(l) == language.Und {
		return ""
	}
	return language.Tag(l).String()
}

// IsRoot возвращает true, если локаль не определена (language.Und).
func (l Locale) IsRoot() bool {
	return language.Tag(l).IsRoot()
}

// ── Парсинг ──────────────────────────────────────────────────

// Parse парсит строковое представление языкового тега.
// Пустая строка интерпретируется как неопределённый язык (language.Und).
func Parse(s string) (Locale, error) {
	if s == "" {
		return Locale(language.Und), nil
	}
	tag, err := language.Parse(s)
	if err != nil {
		return Locale(language.Und), fmt.Errorf("некорректный языковой тег %q: %w", s, err)
	}
	return Locale(tag), nil
}

// MustParse аналогичен Parse, но паникует при ошибке.
func MustParse(s string) Locale {
	l, err := Parse(s)
	if err != nil {
		panic(err)
	}
	return l
}

// ── Сериализация YAML ────────────────────────────────────────

// IsZero реализует yaml.IsZeroer для поддержки omitempty.
func (l Locale) IsZero() bool {
	return l.IsRoot()
}

// UnmarshalYAML парсит языковой тег из YAML.
func (l *Locale) UnmarshalYAML(node *yaml.Node) error {
	var s string
	if err := node.Decode(&s); err != nil {
		return err
	}
	parsed, err := Parse(s)
	if err != nil {
		return err
	}
	*l = parsed
	return nil
}

// MarshalYAML сериализует языковой тег в YAML.
func (l Locale) MarshalYAML() (any, error) {
	return l.String(), nil
}
