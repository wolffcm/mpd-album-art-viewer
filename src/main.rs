use ansi_to_tui::IntoText;
use image::{io::Reader as ImageReader, DynamicImage};
use img_to_ascii::{
    convert::{self, get_conversion_algorithm, get_converter},
    font::Font,
    image::LumaImage,
};
use mpd::{
    client::Client as MpdClient, song::Song, status::State as MpdState, status::Status as MpdStatus,
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
        ExecutableCommand,
    },
    layout::{Alignment, Rect},
    prelude::Backend,
    style::{Modifier, Style},
    symbols::border,
    text::{Span, Text},
    widgets::{
        block::{Position, Title},
        Block, Padding, Paragraph, Widget,
    },
    Frame, Terminal,
};
use std::error::Error;
use std::io::{stdout, Cursor};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() -> Result<()> {
    let mut app = App::create()?;

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let result = app.run(&mut terminal);

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    result
}

#[derive(Default)]
struct State<'a> {
    current_song: Option<Song>,
    status: MpdStatus,
    img: Option<DynamicImage>,
    ascii_img: Option<String>,
    colored_text: Option<Text<'a>>,
}

struct App<'a> {
    client: MpdClient,
    font: Font,
    state: State<'a>,
    last_update_time: Option<Instant>,
    exit: bool,
}

impl App<'_> {
    const UPDATE_PERIOD: Duration = Duration::from_secs(1);
    const ALPHABET: &'static str = include_str!("../alphabets/alphabet.txt");
    const BDF_FILE: &'static str = include_str!("../fonts/bitocra-13.bdf");

    pub fn create() -> Result<Self> {
        let addr = SocketAddr::from(([127, 0, 0, 1], 6600));
        let client = MpdClient::connect(addr)?;
        let alphabet = Self::ALPHABET.chars().collect::<Vec<char>>();
        let font = Font::from_bdf_stream(Self::BDF_FILE.as_bytes(), &alphabet);
        Ok(App {
            font,
            client,
            state: State::default(),
            last_update_time: None,
            exit: false,
        })
    }

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        let _ = self.update_current_song()?;
        terminal.draw(|frame| self.render_frame(frame))?;
        while !self.exit {
            self.handle_events()?;
            terminal.draw(|frame| self.render_frame(frame))?;
        }
        Ok(())
    }

    fn render_frame(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.size())
    }

    fn handle_events(&mut self) -> Result<()> {
        loop {
            if event::poll(Duration::from_millis(5))? {
                match event::read()? {
                    // it's important to check that the event is a key press event as
                    // crossterm also emits key release and repeat events on Windows.
                    Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                        self.handle_key_event(key_event);
                        break;
                    }
                    _ => {}
                };
            }
            if self.elapsed_since_update() >= Self::UPDATE_PERIOD {
                self.update_current_song()?;
                break;
            }
        }
        Ok(())
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Char('q') => self.exit(),
            _ => {}
        }
    }

    fn update_current_song(&mut self) -> Result<()> {
        self.state.status = self.client.status()?;
        let song = self.client.currentsong()?;
        let song_changed = match (&song, &self.state.current_song) {
            (None, None) => false,
            (Some(song0), Some(song1)) if song0 == song1 => false,
            _ => true,
        };

        if song_changed {
            self.state.current_song = song;
            self.state.img = self
                .state
                .current_song
                .as_ref()
                .map(|song| -> Result<DynamicImage> {
                    let art = self.client.albumart(song)?;
                    Ok(ImageReader::new(Cursor::new(art))
                        .with_guessed_format()?
                        .decode()?)
                })
                .transpose()?;
            self.convert_img_to_colored_text()?;
        }

        Ok(())
    }

    fn elapsed_since_update(&self) -> Duration {
        if self.last_update_time.is_none() {
            return Self::UPDATE_PERIOD;
        }

        Instant::now().duration_since(self.last_update_time.unwrap())
    }

    fn song_desc(&self) -> String {
        self.state
            .current_song
            .as_ref()
            .map(|song| {
                format!(
                    "{} - {}",
                    song.artist.as_deref().unwrap_or("Unknown artist"),
                    song.title.as_deref().unwrap_or("Unknown song")
                )
            })
            .unwrap_or("No song playing".to_owned())
    }

    fn fmt_duration(d: &Duration) -> String {
        let s = d.as_secs();
        format!("{:02}:{:02}", s / 60, s % 60)
    }

    fn status_desc(&self) -> String {
        let status = &self.state.status;
        let state = match status.state {
            MpdState::Stop => "Stopped",
            MpdState::Play => "Playing",
            MpdState::Pause => "Paused",
        };
        let times = status
            .time
            .as_ref()
            .map(|(current, total)| format!("{} / {}", Self::fmt_duration(current), Self::fmt_duration(total)));

        match times {
            Some(times) => format!("{} - {}", state, times),
            None => state.to_string()
        }
    }

    fn exit(&mut self) {
        self.exit = true;
    }

    fn convert_img_to_colored_text(&mut self) -> Result<()> {
        self.state.ascii_img = self.state.img.as_ref().map(|img| {
            let rows = convert::img_to_char_rows(
                &self.font,
                &LumaImage::from(img),
                // TODO: store these in App
                get_converter("direction-and-intensity"),
                Some(120),
                0.0,
                &get_conversion_algorithm("edge-augmented"),
            );

            convert::char_rows_to_terminal_color_string(&rows, img)
        });
        self.state.colored_text = self
            .state
            .ascii_img
            .as_deref()
            .map(|str| str.into_text())
            .transpose()?;
        Ok(())
    }
}

impl Widget for &App<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let style = Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD);
        let song_desc: Vec<Span> = vec![
            "".into(),
            Span::styled(self.song_desc(), style),
            "".into(),
        ];
        let state_desc: Vec<Span> = vec![
            "".into(),
            Span::styled(self.status_desc(), style),
            "".into(),
        ];
        let state_desc: Title = state_desc.into();
        let state_desc = state_desc
            .alignment(Alignment::Right)
            .position(Position::Bottom);
        let title: Title = song_desc.into();
        let block = Block::bordered()
            .title(title.alignment(Alignment::Left))
            .title(state_desc)
            .border_set(border::ROUNDED);

        let no_image_text = Text::from("\n\n\nNo image");
        let colored_text = self
            .state
            .colored_text
            .as_ref()
            .map(|text| text)
            .unwrap_or(&no_image_text);

        let padding = Padding {
            left: 2,
            right: 2,
            top: 1,
            bottom: 1,
        };
        let width = 120 + 6;
        let height = 60 + 4;
        let x = (area.width / 2) - (width / 2);
        let y = (area.height / 2) - (height / 2);
        let area = Rect {
            x,
            y,
            width,
            height,
        };

        Paragraph::new(colored_text.clone())
            .centered()
            .block(block.padding(padding))
            .render(area, buf);
    }
}
