package locale

import (
	"testing"

	"golang.org/x/text/language"
	"gopkg.in/yaml.v3"
)

func TestParse(t *testing.T) {
	tests := []struct {
		input   string
		want    language.Tag
		wantErr bool
		wantStr string
	}{
		{"", language.Und, false, ""},
		{"ru", language.Russian, false, "ru"},
		{"ru-RU", language.MustParse("ru-RU"), false, "ru-RU"},
		{"ru_RU", language.MustParse("ru-RU"), false, "ru-RU"},
		{"en-US", language.AmericanEnglish, false, "en-US"},
		{"en_US", language.AmericanEnglish, false, "en-US"},
		{"invalid!!!", language.Und, true, ""},
	}

	for _, tt := range tests {
		t.Run(tt.input, func(t *testing.T) {
			got, err := Parse(tt.input)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("Parse(%q): expected error, got nil", tt.input)
				}
				return
			}
			if err != nil {
				t.Fatalf("Parse(%q): unexpected error: %v", tt.input, err)
			}
			if language.Tag(got) != tt.want {
				t.Errorf("Parse(%q) = %q, want %q", tt.input, language.Tag(got), tt.want)
			}
			if got.String() != tt.wantStr {
				t.Errorf("Parse(%q).String() = %q, want %q", tt.input, got.String(), tt.wantStr)
			}
		})
	}
}

func TestLocale_String(t *testing.T) {
	tests := []struct {
		locale Locale
		want   string
	}{
		{Locale(language.Und), ""},
		{Locale(language.Russian), "ru"},
		{Locale(language.MustParse("ru-RU")), "ru-RU"},
		{Locale(language.English), "en"},
		{Locale(language.AmericanEnglish), "en-US"},
	}

	for _, tt := range tests {
		t.Run(tt.want, func(t *testing.T) {
			if got := tt.locale.String(); got != tt.want {
				t.Errorf("Locale(%q).String() = %q, want %q", language.Tag(tt.locale), got, tt.want)
			}
		})
	}
}

func TestLocale_MarshalYAML(t *testing.T) {
	tests := []struct {
		locale Locale
		want   string
	}{
		{Locale(language.Und), `""` + "\n"},
		{Locale(language.Russian), "ru\n"},
		{Locale(language.MustParse("ru-RU")), "ru-RU\n"},
	}

	for _, tt := range tests {
		t.Run(tt.want, func(t *testing.T) {
			data, err := yaml.Marshal(tt.locale)
			if err != nil {
				t.Fatalf("yaml.Marshal(%q): unexpected error: %v", language.Tag(tt.locale), err)
			}
			if string(data) != tt.want {
				t.Errorf("yaml.Marshal(%q) = %q, want %q", language.Tag(tt.locale), string(data), tt.want)
			}
		})
	}
}

func TestLocale_UnmarshalYAML(t *testing.T) {
	tests := []struct {
		input   string
		want    language.Tag
		wantErr bool
	}{
		{`""`, language.Und, false},
		{"ru", language.Russian, false},
		{"ru-RU", language.MustParse("ru-RU"), false},
		{"ru_RU", language.MustParse("ru-RU"), false},
		{"invalid!!!", language.Und, true},
	}

	for _, tt := range tests {
		t.Run(tt.input, func(t *testing.T) {
			var loc Locale
			err := yaml.Unmarshal([]byte(tt.input), &loc)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("yaml.Unmarshal(%q): expected error, got nil", tt.input)
				}
				return
			}
			if err != nil {
				t.Fatalf("yaml.Unmarshal(%q): unexpected error: %v", tt.input, err)
			}
			if language.Tag(loc) != tt.want {
				t.Errorf("yaml.Unmarshal(%q) = %q, want %q", tt.input, language.Tag(loc), tt.want)
			}
		})
	}
}

func TestLocale_RoundTrip(t *testing.T) {
	tests := []string{"", "ru", "ru-RU", "en-US"}

	for _, input := range tests {
		t.Run(input, func(t *testing.T) {
			loc, err := Parse(input)
			if err != nil {
				t.Fatalf("Parse(%q): unexpected error: %v", input, err)
			}

			data, err := yaml.Marshal(loc)
			if err != nil {
				t.Fatalf("yaml.Marshal(%q): unexpected error: %v", input, err)
			}

			var back Locale
			if err := yaml.Unmarshal(data, &back); err != nil {
				t.Fatalf("yaml.Unmarshal(%q): unexpected error: %v", string(data), err)
			}

			if language.Tag(loc) != language.Tag(back) {
				t.Errorf("round-trip failed: %q -> %q -> %q", input, language.Tag(loc), language.Tag(back))
			}
		})
	}
}

func TestSystem(t *testing.T) {
	tests := []struct {
		name     string
		envLang  string
		envLCAll string
		want     language.Tag
	}{
		{"empty", "", "", language.English},
		{"LANG=ru_RU.UTF-8", "ru_RU.UTF-8", "", language.MustParse("ru-RU")},
		{"LC_ALL=en_US.UTF-8", "", "en_US.UTF-8", language.AmericanEnglish},
		{"LANG=C", "C", "", language.English},
		{"LANG=POSIX", "POSIX", "", language.English},
		{"LANG=ru", "ru", "", language.Russian},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Setenv("LANG", tt.envLang)
			t.Setenv("LC_ALL", tt.envLCAll)

			got := System()
			if language.Tag(got) != tt.want {
				t.Errorf("System() = %q, want %q", language.Tag(got), tt.want)
			}
		})
	}
}
