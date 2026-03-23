package main

import (
	"context"
	"crypto/sha256"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"syscall"

	"github.com/fpvladder/laps/host/internal/config"
	"github.com/spf13/cobra"
	"golang.org/x/text/language"
)

const maxTTSCacheSize = 200 * 1024

// Context key for language
type langKey struct{}

// WithLang returns a new context with language set
func WithLang(ctx context.Context, lang language.Tag) context.Context {
	return context.WithValue(ctx, langKey{}, lang)
}

// LangFromContext extracts language from context, returns empty tag if not set
func LangFromContext(ctx context.Context) language.Tag {
	if lang, ok := ctx.Value(langKey{}).(language.Tag); ok {
		return lang
	}
	return language.Und
}

// Speak synthesizes and plays text using Google TTS
func Speak(ctx context.Context, text string) error {
	cfg, err := config.Load()
	if err != nil {
		slog.Warn("failed to load config", "err", err)
		cfg = config.Default()
	}

	lang := LangFromContext(ctx)
	if lang == language.Und {
		lang = cfg.Lang
	}
	if lang == language.Und {
		lang = language.Russian
	}

	// Get base language code (e.g., "ru" from "ru-RU")
	base, _ := lang.Base()
	langCode := base.String()

	if err := cleanupCache(maxTTSCacheSize); err != nil {
		slog.Warn("cache cleanup failed", "err", err)
	}

	cacheFile, err := getTTSCachePath(text, langCode)
	if err != nil {
		return fmt.Errorf("failed to get cache path: %w", err)
	}

	if _, err := os.Stat(cacheFile); os.IsNotExist(err) {
		slog.Debug("downloading TTS audio")
		if err := downloadTTS(ctx, text, langCode, cacheFile); err != nil {
			if ctx.Err() != nil {
				return context.Canceled
			}
			return fmt.Errorf("failed to download TTS: %w", err)
		}
	} else {
		slog.Debug("using cached audio", "file", cacheFile)
	}

	if err := playAudio(ctx, cacheFile); err != nil {
		if ctx.Err() != nil {
			return context.Canceled
		}
		return fmt.Errorf("failed to play audio: %w", err)
	}

	return nil
}

func runSpeak(cmd *cobra.Command, args []string) {
	// Create cancellable context
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	// Handle interrupt signals
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-sigCh
		slog.Info("received interrupt signal, cancelling...")
		cancel()
	}()

	if len(args) == 0 {
		slog.Warn("no text provided for speak command")
		return
	}

	text := strings.Join(args, " ")
	slog.Debug("text to speak", "text", text)

	// Get lang from flag and store in context
	if langStr, _ := cmd.Flags().GetString("lang"); langStr != "" {
		if lang, err := language.Parse(langStr); err == nil {
			ctx = WithLang(ctx, lang)
		}
	}

	if err := Speak(ctx, text); err != nil {
		if err == context.Canceled {
			slog.Info("speak cancelled")
			return
		}
		slog.Error("speak failed", "err", err)
		fmt.Fprintln(os.Stderr, "Установите ffmpeg: https://ffmpeg.org/download.html")
	}
}

func getTTSCacheDir() (string, error) {
	var cacheDir string
	if xdgCache := os.Getenv("XDG_CACHE_HOME"); xdgCache != "" {
		cacheDir = filepath.Join(xdgCache, "laps", "tts")
	} else {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		cacheDir = filepath.Join(home, ".cache", "laps", "tts")
	}
	if err := os.MkdirAll(cacheDir, 0755); err != nil {
		return "", err
	}
	return cacheDir, nil
}

func getTTSCachePath(text, lang string) (string, error) {
	hash := sha256.Sum256([]byte(lang + ":" + text))
	cacheName := fmt.Sprintf("%x.mp3", hash)
	cacheDir, err := getTTSCacheDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(cacheDir, cacheName), nil
}

func cleanupCache(maxSize int64) error {
	cacheDir, err := getTTSCacheDir()
	if err != nil {
		return err
	}

	entries, err := os.ReadDir(cacheDir)
	if err != nil {
		return err
	}

	type fileInfo struct {
		path    string
		size    int64
		modTime int64
	}

	var files []fileInfo
	var totalSize int64

	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".mp3") {
			continue
		}
		info, err := entry.Info()
		if err != nil {
			continue
		}
		files = append(files, fileInfo{
			path:    filepath.Join(cacheDir, entry.Name()),
			size:    info.Size(),
			modTime: info.ModTime().Unix(),
		})
		totalSize += info.Size()
	}

	if totalSize <= maxSize {
		return nil
	}

	sort.Slice(files, func(i, j int) bool {
		return files[i].modTime < files[j].modTime
	})

	for _, f := range files {
		if totalSize <= maxSize {
			break
		}
		if err := os.Remove(f.path); err == nil {
			totalSize -= f.size
		}
	}

	slog.Debug("cache cleaned", "remaining_size", totalSize)
	return nil
}

func downloadTTS(ctx context.Context, text, lang, outputPath string) error {
	u := fmt.Sprintf("https://translate.google.com/translate_tts?ie=UTF-8&q=%s&tl=%s&client=tw-ob",
		url.QueryEscape(text), lang)

	req, err := http.NewRequestWithContext(ctx, "GET", u, nil)
	if err != nil {
		return err
	}
	req.Header.Set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")

	client := &http.Client{}
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode != 200 {
		return fmt.Errorf("HTTP %d", resp.StatusCode)
	}

	out, err := os.Create(outputPath)
	if err != nil {
		return err
	}
	defer out.Close()

	_, err = io.Copy(out, resp.Body)
	return err
}

func playAudio(ctx context.Context, path string) error {
	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "darwin":
		cmd = exec.CommandContext(ctx, "afplay", path)
		if err := cmd.Run(); err == nil {
			return nil
		}
		cmd = exec.CommandContext(ctx, "ffplay", "-nodisp", "-autoexit", path)
	case "windows":
		cmd = exec.CommandContext(ctx, "powershell", "-c",
			fmt.Sprintf("(New-Object Media.SoundPlayer '%s').PlaySync()", path))
	default:
		cmd = exec.CommandContext(ctx, "mpg123", "-q", path)
		if err := cmd.Run(); err == nil {
			return nil
		}
		cmd = exec.CommandContext(ctx, "ffplay", "-nodisp", "-autoexit", path)
	}
	return cmd.Run()
}
