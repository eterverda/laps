package gui

import (
	"os"
	"runtime"

	"gioui.org/app"
	"gioui.org/op"
	"gioui.org/unit"
	"github.com/fpvladder/laps/host/internal/gui/state"
)

func Run() error {
	errCh := make(chan error, 1)
	go func() {
		errCh <- mainWindow()
		os.Exit(0)
	}()
	runtime.LockOSThread()
	app.Main()
	return <-errCh
}

func mainWindow() error {
	w := new(app.Window)
	w.Option(
		app.Title("Laps"),
		app.Size(unit.Dp(1280), unit.Dp(720)),
	)

	m := state.NewMachine(w.Invalidate)

	var ops op.Ops
	entered := false
	for {
		switch e := w.Event().(type) {
		case app.DestroyEvent:
			m.State.Exit()
			return e.Err
		case app.FrameEvent:
			if !entered {
				entered = true
				m.State.Enter()
			}
			gtx := app.NewContext(&ops, e)
			m.State.Layout(gtx)
			e.Frame(gtx.Ops)
		}
	}
}
