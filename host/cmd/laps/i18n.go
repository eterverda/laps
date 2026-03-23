package main

import (
	"embed"

	"github.com/nicksnyder/go-i18n/v2/i18n"
	"golang.org/x/text/language"
	"gopkg.in/yaml.v3"
)

//go:embed i18n.*.yaml
var localeFS embed.FS

var i18nBundle *i18n.Bundle
var i18nLocalizer *i18n.Localizer

func initI18n(lang language.Tag) {
	if i18nBundle == nil {
		i18nBundle = i18n.NewBundle(language.Russian)
		i18nBundle.RegisterUnmarshalFunc("yaml", yaml.Unmarshal)

		files, err := localeFS.ReadDir(".")
		if err == nil {
			for _, f := range files {
				data, err := localeFS.ReadFile(f.Name())
				if err == nil {
					i18nBundle.ParseMessageFileBytes(data, f.Name())
				}
			}
		}
	}

	if lang == language.Und {
		lang = language.Russian
	}
	i18nLocalizer = i18n.NewLocalizer(i18nBundle, lang.String())
}

func T(key string) string {
	if i18nLocalizer == nil {
		return key
	}
	msg, err := i18nLocalizer.Localize(&i18n.LocalizeConfig{
		MessageID: key,
	})
	if err != nil {
		return key
	}
	return msg
}
