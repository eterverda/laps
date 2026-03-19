package main

import (
	"fmt"
	"os"

	"github.com/fpvladder/laps/host/driver/rssi"
	"github.com/spf13/cobra"
)

var rootCmd = &cobra.Command{
	Use:   "laps",
	Short: "FPV Lap Timer",
	Long:  "Laps — open-source lap timing system for FPV drone racing",
}

var helloCmd = &cobra.Command{
	Use:   "hello",
	Short: "Print hello message",
	Run: func(cmd *cobra.Command, args []string) {
		fmt.Println("Hello Laps")
	},
}

var rssiCmd = &cobra.Command{
	Use:   "rssi",
	Short: "RSSI sensor hello",
	Run: func(cmd *cobra.Command, args []string) {
		fmt.Println(rssi.Hello())
	},
}

func init() {
	rootCmd.AddCommand(helloCmd)
	rootCmd.AddCommand(rssiCmd)
}

func main() {
	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
