package gui

import (
	"image"
	"image/color"

	"gioui.org/layout"
	"gioui.org/op"
	"gioui.org/op/paint"
	"gioui.org/widget/material"

	"github.com/fpvladder/laps/host/driver/webcam"
	"github.com/fpvladder/laps/host/internal/gui/views"
)

type JumbotronState struct {
	th     *material.Theme
	webcam *webcam.Webcam
}

func NewJumbotronState(th *material.Theme, webcam *webcam.Webcam) *JumbotronState {
	return &JumbotronState{
		th:     th,
		webcam: webcam,
	}
}

func (s *JumbotronState) Layout(gtx layout.Context) layout.Dimensions {
	// Black background
	paint.ColorOp{Color: color.NRGBA{A: 255}}.Add(gtx.Ops)
	paint.PaintOp{}.Add(gtx.Ops)

	frame, frameTime, err := s.webcam.Frame()
	if frameTime >= 0 {
		gtx.Execute(op.InvalidateCmd{At: gtx.Now.Add(frameTime)})
	}

	grid := views.NewGrid(s.th)
	grid.AnchorColor = color.NRGBA{R: 255, G: 255, B: 255, A: 31}
	grid.AnchorRows = 5
	grid.AnchorCols = 10

	// 4 viewfinders in 2x2 grid with gaps
	// Each VF: 32 cols x 18 rows (16:9 at charW=16, charH=32)
	// Gaps: 4 cols horizontal, 4 rows vertical
	// Layout:
	//   left=12, gap=4, right=48
	//   top=4, gap=4, bottom=26
	views.Aspect{
		Size: image.Pt(viewW, viewH),
	}.Layout(gtx, func(gtx layout.Context) layout.Dimensions {
		return grid.Layout(gtx,
			views.Cell(image.Rect(15, 4, 79, 22), func(gtx layout.Context) layout.Dimensions {
				return views.Viewfinder{MinX: 0.0, MinY: 0.0, MaxX: 0.5, MaxY: 0.5}.Layout(gtx, frame, err)
			}),
			views.Cell(image.Rect(81, 4, 145, 22), func(gtx layout.Context) layout.Dimensions {
				return views.Viewfinder{MinX: 0.5, MinY: 0.0, MaxX: 1.0, MaxY: 0.5}.Layout(gtx, frame, err)
			}),
			views.Cell(image.Rect(15, 23, 79, 41), func(gtx layout.Context) layout.Dimensions {
				return views.Viewfinder{MinX: 0.0, MinY: 0.5, MaxX: 0.5, MaxY: 1.0}.Layout(gtx, frame, err)
			}),
			views.Cell(image.Rect(81, 23, 145, 41), func(gtx layout.Context) layout.Dimensions {
				return views.Viewfinder{MinX: 0.5, MinY: 0.5, MaxX: 1.0, MaxY: 1.0}.Layout(gtx, frame, err)
			}),
		)
	})

	return layout.Dimensions{Size: gtx.Constraints.Max}
}
