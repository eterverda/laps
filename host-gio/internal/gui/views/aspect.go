package views

import (
	"image"
	"image/color"

	"gioui.org/f32"
	"gioui.org/layout"
	"gioui.org/op"
	"gioui.org/op/clip"
	"gioui.org/op/paint"
)

type Aspect struct {
	Size        image.Point
	BorderColor color.NRGBA
	BorderWidth float32
}

func (a Aspect) Layout(gtx layout.Context, widget layout.Widget) layout.Dimensions {
	if a.Size.X == 0 || a.Size.Y <= 0 {
		return layout.Dimensions{Size: gtx.Constraints.Max}
	}

	w := float32(a.Size.X)
	h := float32(a.Size.Y)

	winW := float32(gtx.Constraints.Max.X)
	winH := float32(gtx.Constraints.Max.Y)
	aspect := w / h
	winAspect := winW / winH

	var scale, offsetX, offsetY float32
	if winAspect > aspect {
		scale = winH / h
		offsetX = (winW - w*scale) / 2
		offsetY = 0
	} else {
		scale = winW / w
		offsetX = 0
		offsetY = (winH - h*scale) / 2
	}

	// Draw border in window coordinates
	if a.BorderColor.A > 0 && a.BorderWidth > 0 {
		paint.ColorOp{Color: a.BorderColor}.Add(gtx.Ops)
		stroke := clip.Stroke{
			Path: clip.Rect(image.Rect(
				round32(offsetX-a.BorderWidth),
				round32(offsetY-a.BorderWidth),
				round32(offsetX+w*scale+a.BorderWidth),
				round32(offsetY+h*scale+a.BorderWidth),
			)).Path(),
			Width: a.BorderWidth,
		}
		border := stroke.Op().Push(gtx.Ops)
		paint.PaintOp{}.Add(gtx.Ops)
		border.Pop()
	}

	// Apply transform: scale then offset
	transform := f32.Affine2D{}.
		Scale(f32.Point{}, f32.Point{X: scale, Y: scale}).
		Offset(f32.Point{X: offsetX, Y: offsetY})
	stack := op.Affine(transform).Push(gtx.Ops)
	defer stack.Pop()

	// Call widget with virtual constraints
	childGtx := gtx
	childGtx.Constraints = layout.Exact(a.Size)
	widget(childGtx)

	return layout.Dimensions{Size: gtx.Constraints.Max}
}

func round32(x float32) int {
	if x < 0 {
		return int(x - 0.5)
	}
	return int(x + 0.5)
}
