package gui

import (
	"os"
	"runtime"

	"gioui.org/app"
	"gioui.org/op"
	"gioui.org/unit"
	"gioui.org/widget/material"
	"github.com/fpvladder/laps/host/driver/webcam"
)

func Run() error {
	th := NewTheme()
	webcam := webcam.New(webcamWidth, webcamHeight, webcamDeviceLabel)

	errCh := make(chan error, 1)
	go func() {
		errCh <- mainWindow(th, webcam)
		os.Exit(0)
	}()

	runtime.LockOSThread()
	app.Main()
	return <-errCh
}

func mainWindow(th *material.Theme, webcam *webcam.Webcam) error {
	w := new(app.Window)
	w.Option(
		app.Title("Laps"),
		app.Size(unit.Dp(1280), unit.Dp(720)),
	)

	s := NewOnlyState(th, webcam)

	var ops op.Ops
	for {
		switch e := w.Event().(type) {
		case app.DestroyEvent:
			return e.Err
		case app.FrameEvent:
			gtx := app.NewContext(&ops, e)
			s.Layout(gtx)
			e.Frame(gtx.Ops)
		}
	}
}

func jumbotronWindow(th *material.Theme, webcam *webcam.Webcam) {
	w := new(app.Window)
	w.Option(
		app.Title("Laps — Jumbotron"),
		app.Size(unit.Dp(960), unit.Dp(640)),
	)

	s := NewJumbotronState(th, webcam)

	var ops op.Ops
	for {
		switch e := w.Event().(type) {
		case app.DestroyEvent:
			return
		case app.FrameEvent:
			gtx := app.NewContext(&ops, e)
			s.Layout(gtx)
			e.Frame(gtx.Ops)
		}
	}
}
