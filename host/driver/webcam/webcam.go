package webcam

import (
	"errors"
	"fmt"
	"image"
	"image/draw"
	"strings"
	"sync"
	"sync/atomic"

	"github.com/pion/mediadevices"
	_ "github.com/pion/mediadevices/pkg/driver/camera"
	"github.com/pion/mediadevices/pkg/frame"
	"github.com/pion/mediadevices/pkg/prop"
)

var ErrNoFrame = errors.New("no frame available")

// Webcam captures video frames from a camera device.
type Webcam struct {
	mu          sync.RWMutex
	frame       *image.RGBA
	err         error
	width       int
	height      int
	deviceLabel string
	clients     map[int]func()
	nextID      int
	refCount    atomic.Int32
}

// New creates a new webcam that captures at the given resolution.
// If deviceLabel is empty, uses the first available device.
func New(width, height int, deviceLabel string) *Webcam {
	return &Webcam{
		width:       width,
		height:      height,
		deviceLabel: deviceLabel,
		clients:     make(map[int]func()),
	}
}

// Retain registers a client and starts capture if this is the first retain.
// Returns a client ID that must be passed to Release.
func (w *Webcam) Retain(c func()) int {
	w.mu.Lock()
	id := w.nextID
	w.nextID++
	w.clients[id] = c
	w.mu.Unlock()
	if w.refCount.Add(1) == 1 {
		go w.run()
	}
	return id
}

// Release unregisters a client by ID and stops capture if this was the last release.
func (w *Webcam) Release(id int) {
	w.mu.Lock()
	delete(w.clients, id)
	w.mu.Unlock()
	if w.refCount.Add(-1) == 0 {
		w.mu.Lock()
		w.clients = make(map[int]func())
		w.mu.Unlock()
	}
}

func (w *Webcam) notify() {
	w.mu.RLock()
	clients := make([]func(), 0, len(w.clients))
	for _, c := range w.clients {
		clients = append(clients, c)
	}
	w.mu.RUnlock()
	for _, c := range clients {
		c()
	}
}

func (w *Webcam) run() {
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
		if w.refCount.Load() == 0 {
			fmt.Println("[webcam] capture stopped")
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

		w.mu.Lock()
		w.frame = rgba
		w.err = nil
		w.mu.Unlock()

		w.notify()
	}
}

func (w *Webcam) setError(err error) {
	w.mu.Lock()
	w.err = err
	w.mu.Unlock()
	fmt.Printf("[webcam] error: %v\n", err)
	w.notify()
}

// Frame returns the latest captured frame and any error.
// Returns ErrNoFrame if no frame has been captured yet.
func (w *Webcam) Frame() (*image.RGBA, error) {
	w.mu.RLock()
	defer w.mu.RUnlock()
	if w.err != nil {
		return nil, w.err
	}
	if w.frame == nil {
		return nil, ErrNoFrame
	}
	return w.frame, nil
}
