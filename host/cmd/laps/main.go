package main

import (
	"fmt"
	"log/slog"
	"os"
	"strings"

	"github.com/fpvladder/laps/host/internal/config"
	"github.com/spf13/cobra"
	"golang.org/x/text/language"
)

func main() {
	/* pre-parse --lang flag before cobra initialization */
	langStr := ""
	for i, arg := range os.Args {
		if arg == "--lang" && i+1 < len(os.Args) {
			langStr = os.Args[i+1]
			break
		}
		if strings.HasPrefix(arg, "--lang=") {
			langStr = strings.TrimPrefix(arg, "--lang=")
			break
		}
	}
	/* end pre-parse */

	var lang language.Tag
	if langStr != "" {
		lang, _ = language.Parse(langStr)
	}
	if lang == language.Und {
		if cfg, err := config.Load(); err == nil {
			lang = cfg.Lang
		}
	}
	if lang == language.Und {
		lang = language.Russian
	}
	initI18n(lang)

	root := &cobra.Command{
		Use:   T("cmd.laps.use"),
		Short: T("cmd.laps.short"),
		Long:  T("cmd.laps.long"),
		RunE: func(cmd *cobra.Command, args []string) error {
			return cmd.Help()
		},
	}

	root.PersistentFlags().String("lang", "", T("cmd.laps.flags.lang.usage"))
	root.PersistentFlags().String("loglevel", "warn", T("cmd.laps.flags.loglevel.usage"))

	root.AddCommand(&cobra.Command{
		Use:   T("cmd.hello.use"),
		Short: T("cmd.hello.short"),
		Long:  T("cmd.hello.long"),
		Run: func(cmd *cobra.Command, args []string) {
			fmt.Println("Hello, Laps!")
		},
	})

	root.AddCommand(&cobra.Command{
		Use:   T("cmd.rssi.use"),
		Short: T("cmd.rssi.short"),
		Long:  T("cmd.rssi.long"),
		Run: func(cmd *cobra.Command, args []string) {
			fmt.Println("Hello from RSSI sensor")
		},
	})

	root.AddCommand(&cobra.Command{
		Use:   T("cmd.speak.use"),
		Short: T("cmd.speak.short"),
		Long:  T("cmd.speak.long"),
		Run:   runSpeak,
	})

	if err := root.Execute(); err != nil {
		slog.Error("execution error", "err", err)
		os.Exit(1)
	}
}
