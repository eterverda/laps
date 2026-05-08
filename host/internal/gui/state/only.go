package state

import (
	"image"
	"image/color"
	"time"

	"gioui.org/layout"
	"gioui.org/op"
	"gioui.org/op/paint"
	"gioui.org/widget/material"

	"github.com/fpvladder/laps/host/driver/webcam"
	"github.com/fpvladder/laps/host/internal/gui/views"
)

const (
	webcamWidth       = 1920
	webcamHeight      = 1080
	webcamDeviceLabel = "0x1200000345f2131"
)

type OnlyState struct {
	th           *material.Theme
	webcam       *webcam.Webcam
	invalidate   func()
	webcamClient int
}

func NewOnlyState(th *material.Theme, invalidate func()) *OnlyState {
	return &OnlyState{
		th:         th,
		webcam:     webcam.New(webcamWidth, webcamHeight, webcamDeviceLabel),
		invalidate: invalidate,
	}
}

func (s *OnlyState) Enter() {
	if s.webcamClient != 0 {
		return
	}
	s.webcamClient = s.webcam.Retain(s.invalidate)
}

func (s *OnlyState) Exit() {
	if s.webcamClient == 0 {
		return
	}
	s.webcam.Release(s.webcamClient)
	s.webcamClient = 0
}

func (s *OnlyState) Layout(gtx layout.Context) layout.Dimensions {
	// Black background
	paint.ColorOp{Color: color.NRGBA{A: 255}}.Add(gtx.Ops)
	paint.PaintOp{}.Add(gtx.Ops)

	frame, err := s.webcam.Frame()

	grid := views.NewGrid(s.th)
	grid.AnchorColor = color.NRGBA{R: 255, G: 255, B: 255, A: 31}
	grid.AnchorRows = 5
	grid.AnchorCols = 10

	// 4 viewfinders in a row: gap 4, vf 34x20, gap 4, vf 34x20, gap 4, vf 34x20, gap 4, vf 34x20, gap 4
	// Total: 4 + 34 + 4 + 34 + 4 + 34 + 4 + 34 + 4 = 160 cells wide
	// Height 20 cells, positioned at row 10
	views.Aspect{
		Size:        image.Pt(viewW, viewH),
		BorderColor: color.NRGBA{R: 255, G: 255, B: 255, A: 15},
		BorderWidth: 1,
	}.Layout(gtx, func(gtx layout.Context) layout.Dimensions {
		return grid.Layout(gtx,
			views.Cell(image.Rect(148, 0, 160, 1), func(gtx layout.Context) layout.Dimensions {
				now := time.Now()
				date := now.Format("\U000f00ed 2006/01/02")
				lbl := material.Label(s.th, s.th.TextSize, date)
				lbl.Color = color.NRGBA{R: 255, G: 255, B: 255, A: 255}
				return lbl.Layout(gtx)
			}),
			views.Cell(image.Rect(148, 1, 160, 2), func(gtx layout.Context) layout.Dimensions {
				lbl := material.Label(s.th, s.th.TextSize, "\uf450 СПб")
				lbl.Color = color.NRGBA{R: 255, G: 255, B: 255, A: 255}
				return lbl.Layout(gtx)
			}),
			views.Cell(image.Rect(126, 0, 148, 2), func(gtx layout.Context) layout.Dimensions {
				now := time.Now()
				clock := now.Format("\uf017 15:04:05")
				lbl := material.Label(s.th, s.th.TextSize*2, clock)
				lbl.Color = color.NRGBA{R: 255, G: 255, B: 255, A: 255}

				// Schedule next redraw at the start of next second
				next := now.Truncate(time.Second).Add(time.Second)
				gtx.Execute(op.InvalidateCmd{At: next})
				return lbl.Layout(gtx)
			}),
			views.Cell(image.Rect(10, 3, 42, 12), func(gtx layout.Context) layout.Dimensions {
				return views.Viewfinder{MinX: 0.0, MinY: 0.0, MaxX: 0.5, MaxY: 0.5}.Layout(gtx, frame, err)
			}),
			views.Cell(image.Rect(46, 3, 78, 12), func(gtx layout.Context) layout.Dimensions {
				return views.Viewfinder{MinX: 0.5, MinY: 0.0, MaxX: 1.0, MaxY: 0.5}.Layout(gtx, frame, err)
			}),
			views.Cell(image.Rect(82, 3, 114, 12), func(gtx layout.Context) layout.Dimensions {
				return views.Viewfinder{MinX: 0.0, MinY: 0.5, MaxX: 0.5, MaxY: 1.0}.Layout(gtx, frame, err)
			}),
			views.Cell(image.Rect(118, 3, 150, 12), func(gtx layout.Context) layout.Dimensions {
				return views.Viewfinder{MinX: 0.5, MinY: 0.5, MaxX: 1.0, MaxY: 1.0}.Layout(gtx, frame, err)
			}),
		)
	})

	return layout.Dimensions{Size: gtx.Constraints.Max}
}
