package webcam

import (
	"errors"
	"fmt"
	"image"
	"image/draw"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/pion/mediadevices"
	_ "github.com/pion/mediadevices/pkg/driver/camera"
	"github.com/pion/mediadevices/pkg/frame"
	"github.com/pion/mediadevices/pkg/prop"
)

var ErrNoFrame = errors.New("no frame available")

const idleTimeout = 5 * time.Second

// Webcam captures video frames from a camera device.
type Webcam struct {
	mu          sync.RWMutex
	frame       *image.RGBA
	err         error
	width       int
	height      int
	deviceLabel string
	lastRead    atomic.Int64 // unix nanoseconds
	running     atomic.Bool
	frameTime   time.Duration // frame interval based on fps
}

// New creates a new webcam that captures at the given resolution.
// If deviceLabel is empty, uses the first available device.
func New(width, height int, deviceLabel string) *Webcam {
	return &Webcam{
		width:       width,
		height:      height,
		deviceLabel: deviceLabel,
	}
}

func (w *Webcam) touch() {
	w.lastRead.Store(time.Now().UnixNano())
}

// Frame returns the latest captured frame, the recommended frame interval, and any error.
// The frame interval is based on the configured frame rate (e.g. 30fps = ~33ms).
// Returns -1 as interval if no frame is available yet or on error.
func (w *Webcam) Frame() (*image.RGBA, time.Duration, error) {
	w.touch()
	if w.running.CompareAndSwap(false, true) {
		go w.run()
	}

	w.mu.RLock()
	defer w.mu.RUnlock()
	if w.err != nil {
		return nil, -1, w.err
	}
	if w.frame == nil {
		return nil, -1, ErrNoFrame
	}
	return w.frame, w.frameTime, nil
}

func (w *Webcam) run() {
	for {
		w.capture()

		// Wait until someone calls Frame() again
		w.running.Store(false)
		for {
			time.Sleep(100 * time.Millisecond)
			if w.running.Load() {
				break
			}
		}
	}
}

func (w *Webcam) capture() {
	devices := mediadevices.EnumerateDevices()
	if len(devices) == 0 {
		w.setError(errors.New("no capture devices found"))
		return
	}

	var devID, devLabel string
	for _, device := range devices {
		if w.deviceLabel != "" && (strings.Contains(device.Label, w.deviceLabel) || strings.Contains(device.DeviceID, w.deviceLabel)) {
			devID = device.DeviceID
			devLabel = device.Label
			break
		}
	}
	if devID == "" {
		devID = devices[0].DeviceID
		devLabel = devices[0].Label
		fmt.Printf("[webcam] no match, falling back to first device\n")
	}
	fmt.Printf("[webcam] opening device: %s\n", devLabel)

	stream, err := mediadevices.GetUserMedia(mediadevices.MediaStreamConstraints{
		Video: func(c *mediadevices.MediaTrackConstraints) {
			c.DeviceID = prop.String(devID)
			c.Width = prop.Int(w.width)
			c.Height = prop.Int(w.height)
			c.FrameRate = prop.Float(30)
			w.frameTime = time.Second / 30
			c.FrameFormat = prop.FrameFormat(frame.FormatRGBA)
		},
	})
	if err != nil {
		w.setError(fmt.Errorf("GetUserMedia: %w", err))
		return
	}

	tracks := stream.GetTracks()
	if len(tracks) == 0 {
		w.setError(errors.New("no tracks"))
		return
	}

	videoTrack := tracks[0].(*mediadevices.VideoTrack)
	defer videoTrack.Close()

	reader := videoTrack.NewReader(false)

	fmt.Println("[webcam] capture started")
	for {
		if time.Since(time.Unix(0, w.lastRead.Load())) > idleTimeout {
			fmt.Println("[webcam] capture stopped (idle)")
			return
		}

		img, release, err := reader.Read()
		if err != nil {
			w.setError(fmt.Errorf("read: %w", err))
			continue
		}

		bounds := img.Bounds()
		rgba := image.NewRGBA(bounds)
		draw.Draw(rgba, bounds, img, bounds.Min, draw.Src)
		release()

		now := time.Now()
		w.mu.Lock()
		if w.frame != nil {
			w.frameTime = now.Sub(time.Unix(0, w.lastRead.Load()))
		} else {
			w.frameTime = -1
		}
		w.frame = rgba
		w.err = nil
		w.mu.Unlock()
	}
}

func (w *Webcam) setError(err error) {
	w.mu.Lock()
	w.err = err
	w.mu.Unlock()
	fmt.Printf("[webcam] error: %v\n", err)
}
