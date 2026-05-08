package state

import (
	"image"
	"image/color"
	"time"

	"gioui.org/layout"
	"gioui.org/op"
	"gioui.org/op/paint"
	"gioui.org/widget/material"
	"github.com/fpvladder/laps/host/internal/gui/views"
)

type OnlyState struct {
	th *material.Theme
}

func NewOnlyState(th *material.Theme) *OnlyState {
	return &OnlyState{th: th}
}

func (s *OnlyState) Layout(gtx layout.Context) layout.Dimensions {
	// Black background
	paint.ColorOp{Color: color.NRGBA{A: 255}}.Add(gtx.Ops)
	paint.PaintOp{}.Add(gtx.Ops)

	grid := views.NewGrid(s.th)
	grid.AnchorColor = color.NRGBA{R: 255, G: 255, B: 255, A: 31}
	grid.AnchorRows = 5
	grid.AnchorCols = 10

	// Virtual view with 2560x1440 virtual size
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
		)
	})

	return layout.Dimensions{Size: gtx.Constraints.Max}
}
