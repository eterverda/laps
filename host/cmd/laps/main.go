package main

import (
	"fmt"
	"log/slog"
	"os"

	"github.com/fpvladder/laps/host/internal/config"
	"github.com/fpvladder/laps/host/internal/locale"
	"github.com/spf13/cobra"
	"gopkg.in/yaml.v3"
)

func main() {
	parsedConfig := parseConfig()
	cfg := config.Default().Override(parsedConfig)

	initI18n(cfg.Locale)

	fmt.Printf("// %s\n", T("cli.logs.parsed-config"))
	yaml.NewEncoder(os.Stdout).Encode(parsedConfig)
	fmt.Println()

	fmt.Printf("// %s\n", T("cli.logs.result-config"))
	yaml.NewEncoder(os.Stdout).Encode(cfg)
	fmt.Println()

	root := &cobra.Command{
		Use:   T("cmd.laps.use"),
		Short: T("cmd.laps.short"),
		Long:  T("cmd.laps.long"),
		RunE: func(cmd *cobra.Command, args []string) error {
			return cmd.Help()
		},
	}

	root.PersistentFlags().String("locale", "", T("cmd.laps.flags.locale.usage"))
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

// parseConfig extracts --locale and --config-file from raw args using cobra,
// loads config and returns it. Unknown flags are ignored.
func parseConfig() *config.Config {
	cmd := &cobra.Command{
		FParseErrWhitelist: cobra.FParseErrWhitelist{UnknownFlags: true},
	}
	cmd.PersistentFlags().String("locale", "", "")
	cmd.PersistentFlags().String("config-file", "", "")
	cmd.Execute()

	localeStr, _ := cmd.Flags().GetString("locale")
	configFile, _ := cmd.Flags().GetString("config-file")

	cfg, _ := config.Load(configFile)
	if localeStr != "" {
		if loc, err := locale.Parse(localeStr); err == nil {
			cfg.Locale = loc
		}
	}
	return cfg
}
