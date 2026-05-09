package views

import (
	"image"
	"image/color"
	"math"

	"gioui.org/f32"
	"gioui.org/layout"
	"gioui.org/op"
	"gioui.org/op/clip"
	"gioui.org/op/paint"
	"github.com/fpvladder/laps/host/assets"
)

// Viewfinder displays a cropped region of a video frame.
// It is stateless — the frame and error are passed to Layout.
// Crop is specified as normalized coordinates [0,1] defining
// the min and max corners of the crop rectangle:
//
//	minX, minY = top-left corner
//	maxX, maxY = bottom-right corner
//
// (0,0,1,1) = full frame, (0.5,0.5,1,1) = bottom-right quarter
type Viewfinder struct {
	MinX float32
	MinY float32
	MaxX float32
	MaxY float32
}

// Layout draws the cropped frame scaled to fit the available space.
// If frame is nil or err is not nil, draws a red X placeholder.
func (v Viewfinder) Layout(gtx layout.Context, frame *image.RGBA, err error) layout.Dimensions {
	max := gtx.Constraints.Max
	if max.X == 0 || max.Y == 0 {
		return layout.Dimensions{Size: max}
	}

	if frame != nil && err == nil {
		// Calculate crop bounds in pixels
		bounds := frame.Bounds()
		fw := float32(bounds.Dx())
		fh := float32(bounds.Dy())

		cropMinX := int(v.MinX * fw)
		cropMinY := int(v.MinY * fh)
		cropMaxX := int(v.MaxX * fw)
		cropMaxY := int(v.MaxY * fh)

		// Clamp to frame bounds
		if cropMinX < 0 {
			cropMinX = 0
		}
		if cropMinY < 0 {
			cropMinY = 0
		}
		if cropMaxX > bounds.Dx() {
			cropMaxX = bounds.Dx()
		}
		if cropMaxY > bounds.Dy() {
			cropMaxY = bounds.Dy()
		}

		// Create sub-image for the crop region
		cropRect := image.Rect(cropMinX, cropMinY, cropMaxX, cropMaxY)
		cropped := frame.SubImage(cropRect)

		imgOp := paint.NewImageOp(cropped)
		imgOp.Add(gtx.Ops)

		// Scale to fit while preserving aspect ratio
		cropWpx := float32(cropRect.Dx())
		cropHpx := float32(cropRect.Dy())
		scaleX := float32(max.X) / cropWpx
		scaleY := float32(max.Y) / cropHpx
		scale := scaleX
		if scaleY < scaleX {
			scale = scaleY
		}

		newW := cropWpx * scale
		newH := cropHpx * scale
		offsetX := (float32(max.X) - newW) / 2
		offsetY := (float32(max.Y) - newH) / 2

		transform := f32.Affine2D{}.
			Scale(f32.Point{}, f32.Point{X: scale, Y: scale}).
			Offset(f32.Point{X: offsetX, Y: offsetY})

		stack := op.Affine(transform).Push(gtx.Ops)
		paint.PaintOp{}.Add(gtx.Ops)
		stack.Pop()
	} else {
		// Draw testcard SVG placeholder
		drawTestcardPlaceholder(gtx)
	}

	return layout.Dimensions{Size: max}
}

// drawTestcardPlaceholder draws the SMPTE testcard SVG scaled to fit the available space.
func drawTestcardPlaceholder(gtx layout.Context) {
	max := gtx.Constraints.Max

	clipStack := clip.Rect(image.Rect(0, 0, max.X, max.Y)).Push(gtx.Ops)
	defer clipStack.Pop()

	// Black background
	paint.ColorOp{Color: color.NRGBA{A: 255}}.Add(gtx.Ops)
	paint.PaintOp{}.Add(gtx.Ops)

	// Scale SVG to fill the entire viewfinder (stretch to fit)
	// Testcard SVG is 672x504
	svgW := float32(672)
	svgH := float32(504)
	scaleX := float32(max.X) / svgW
	scaleY := float32(max.Y) / svgH

	transform := f32.Affine2D{}.
		Scale(f32.Point{}, f32.Point{X: scaleX, Y: scaleY})

	stack := op.Affine(transform).Push(gtx.Ops)
	assets.Image_testcard_fixed.Call.Add(gtx.Ops)
	stack.Pop()
}

// drawPlaceholder draws a black rectangle with a red X cross.
// Fills exactly the area given by gtx.Constraints, as determined by the parent Grid.
func drawPlaceholder(gtx layout.Context) {
	max := gtx.Constraints.Max

	clipStack := clip.Rect(image.Rect(0, 0, max.X, max.Y)).Push(gtx.Ops)
	defer clipStack.Pop()

	// Black background
	paint.ColorOp{Color: color.NRGBA{A: 255}}.Add(gtx.Ops)
	paint.PaintOp{}.Add(gtx.Ops)

	// Red X
	red := color.NRGBA{R: 255, G: 0, B: 0, A: 255}
	strokeWidth := float32(max.X) / 20
	if strokeWidth < 2 {
		strokeWidth = 2
	}

	// Diagonal from top-left to bottom-right
	drawLine(gtx, f32.Point{X: 0, Y: 0}, f32.Point{X: float32(max.X), Y: float32(max.Y)}, strokeWidth, red)
	// Diagonal from top-right to bottom-left
	drawLine(gtx, f32.Point{X: float32(max.X), Y: 0}, f32.Point{X: 0, Y: float32(max.Y)}, strokeWidth, red)
}

// drawLine draws a line from p1 to p2 with given width and color.
func drawLine(gtx layout.Context, p1, p2 f32.Point, width float32, c color.NRGBA) {
	// Calculate perpendicular vector for line thickness
	dx := p2.X - p1.X
	dy := p2.Y - p1.Y
	length := float32(math.Sqrt(float64(dx*dx + dy*dy)))
	if length == 0 {
		return
	}

	// Normalize and get perpendicular
	nx := -dy / length
	ny := dx / length

	// Half-width offset
	hw := width / 2
	offsetX := nx * hw
	offsetY := ny * hw

	// Quad vertices
	var path clip.Path
	path.Begin(gtx.Ops)
	path.MoveTo(p1.Add(f32.Point{X: offsetX, Y: offsetY}))
	path.LineTo(p2.Add(f32.Point{X: offsetX, Y: offsetY}))
	path.LineTo(p2.Sub(f32.Point{X: offsetX, Y: offsetY}))
	path.LineTo(p1.Sub(f32.Point{X: offsetX, Y: offsetY}))
	path.Close()

	paint.FillShape(gtx.Ops, c, clip.Outline{Path: path.End()}.Op())
}
