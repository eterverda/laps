package main

import (
	"fmt"
	"log/slog"
	"os"

	"github.com/fpvladder/laps/host/internal/config"
	"github.com/spf13/cobra"
	"golang.org/x/text/language"
)

func main() {
	cfg := parseConfig()

	initI18n(cfg.Lang)

	root := &cobra.Command{
		Use:   T("cmd.laps.use"),
		Short: T("cmd.laps.short"),
		Long:  T("cmd.laps.long"),
		RunE: func(cmd *cobra.Command, args []string) error {
			return cmd.Help()
		},
	}

	root.PersistentFlags().String("lang", "", T("cmd.laps.flags.lang.usage"))
	root.PersistentFlags().String("config-file", "", T("cmd.laps.flags.config-file.usage"))
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
		Run: func(cmd *cobra.Command, args []string) {
			runSpeak(cfg, args)
		},
	})

	if err := root.Execute(); err != nil {
		slog.Error("execution error", "err", err)
		os.Exit(1)
	}
}

// parseConfig extracts --lang and --config-file from raw args using cobra,
// loads config and returns it. Unknown flags are ignored.
func parseConfig() *config.Config {
	cmd := &cobra.Command{
		FParseErrWhitelist: cobra.FParseErrWhitelist{UnknownFlags: true},
	}
	cmd.PersistentFlags().String("lang", "", "")
	cmd.PersistentFlags().String("config-file", "", "")
	cmd.Execute()

	langStr, _ := cmd.Flags().GetString("lang")
	configFile, _ := cmd.Flags().GetString("config-file")

	cfg, _ := config.Load(configFile)
	if cfg == nil {
		cfg = config.Default()
	}
	if langStr != "" {
		if tag, err := language.Parse(langStr); err == nil {
			cfg.Lang = tag
		}
	}
	return cfg
}
