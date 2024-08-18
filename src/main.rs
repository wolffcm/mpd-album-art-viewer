use ansi_to_tui::IntoText;
use clap::Parser;
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
use std::{io::{stdout, Cursor}, net::ToSocketAddrs};
use std::time::{Duration, Instant};
use std::{error::Error, thread::JoinHandle};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, value_name = "HOST", default_value = "localhost")]
    host: String,
    #[arg(long, value_name = "PORT", default_value_t = 6600)]
    port: u16,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let host_port = format!("{}:{}", args.host, args.port);

    let mut app = App::create(&host_port)?;

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let result = app.run(&mut terminal);

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    result
}

enum ImgState {
    Idle(Option<(DynamicImage, Text<'static>)>),
    Converting(JoinHandle<Option<(DynamicImage, Text<'static>)>>),
}

impl ImgState {
    fn is_idle(&self) -> bool {
        match self {
            ImgState::Idle(_) => true,
            _ => false,
        }
    }

    fn is_working(&self) -> bool {
        match self {
            ImgState::Idle(_) => false,
            ImgState::Converting(jh) => !jh.is_finished(),
        }
    }

    fn finish_conversion(&mut self) {
        let mut tmp = ImgState::default();
        std::mem::swap(&mut tmp, self);

        let jh = match tmp {
            ImgState::Idle(_) => panic!("not converting"),
            ImgState::Converting(jh) => {
                assert!(jh.is_finished());
                jh
            }
        };

        match jh.join() {
            Err(err) => panic!("{:?}", err),
            Ok(converted) => *self = ImgState::Idle(converted),
        }
    }
}

impl Default for ImgState {
    fn default() -> Self {
        Self::Idle(None)
    }
}

#[derive(Default)]
struct State {
    area: Rect,
    current_song: Option<Song>,
    mpd_status: MpdStatus,
    img_state: ImgState,
}

struct App {
    client: MpdClient,
    font: Font,
    state: State,
    last_update_time: Option<Instant>,
    exit: bool,
}

impl App {
    const UPDATE_PERIOD: Duration = Duration::from_secs(1);
    const ALPHABET: &'static str = include_str!("../alphabets/alphabet.txt");
    const BDF_FILE: &'static str = include_str!("../fonts/bitocra-13.bdf");

    pub fn create(host_port: &str) -> Result<Self> {
        let mut addrs_iter = host_port.to_socket_addrs()?;
        let addr = match  addrs_iter.next() {
            None => return Err("could not resolve host".into()),
            Some(addr) => addr,
        };

        
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
        self.state.area = terminal.get_frame().size();

        let _ = self.update_app_state()?;
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
                self.update_app_state()?;
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

    fn update_app_state(&mut self) -> Result<()> {
        self.state.mpd_status = self.client.status()?;
        let song = self.client.currentsong()?;
        let song_changed = match (&song, &self.state.current_song) {
            (None, None) => false,
            (Some(song0), Some(song1)) if song0 == song1 => false,
            _ => true,
        };

        self.state.current_song = song;

        if song_changed && self.state.img_state.is_idle() {
            // enter converting state
            let art: Option<Vec<u8>> = self
                .state
                .current_song
                .as_ref()
                .map(|song| -> Option<Vec<u8>> { self.client.albumart(song).ok() })
                .flatten();
            let font = self.font.clone();
            let width = (self.state.area.height as usize - 10) * 2;
            let jh = std::thread::spawn(move || -> Option<(DynamicImage, Text<'static>)> {
                let art = match art {
                    None => return None,
                    Some(_art) => _art,
                };

                let dyn_img = ImageReader::new(Cursor::new(art))
                    .with_guessed_format()
                    .ok()?
                    .decode()
                    .ok()?;
                let rows = convert::img_to_char_rows(
                    &font,
                    &LumaImage::from(&dyn_img),
                    get_converter("direction-and-intensity"),
                    Some(width),
                    0.0,
                    &get_conversion_algorithm("edge-augmented"),
                );

                let text = convert::char_rows_to_terminal_color_string(&rows, &dyn_img)
                    .into_text()
                    .ok()?;
                Some((dyn_img, text))
            });
            self.state.img_state = ImgState::Converting(jh);
        } else if self.state.img_state.is_idle() || self.state.img_state.is_working() {
            // Nothing to do
        } else {
            self.state.img_state.finish_conversion();
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
        let status = &self.state.mpd_status;
        let state = match status.state {
            MpdState::Stop => "Stopped",
            MpdState::Play => "Playing",
            MpdState::Pause => "Paused",
        };
        let times = status.time.as_ref().map(|(current, total)| {
            format!(
                "{} / {}",
                Self::fmt_duration(current),
                Self::fmt_duration(total)
            )
        });

        match times {
            Some(times) => format!("{} - {}", state, times),
            None => state.to_string(),
        }
    }

    fn exit(&mut self) {
        self.exit = true;
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title_style = Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD);
        let song_desc: Vec<Span> = vec![
            "".into(),
            Span::styled(self.song_desc(), title_style),
            "".into(),
        ];
        let state_desc: Vec<Span> = vec![
            "".into(),
            Span::styled(self.status_desc(), title_style),
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

        let no_img_style = Style::default().add_modifier(Modifier::DIM);
        let no_image: Text<'static> = Span::styled("No image", no_img_style).into();
        let converting_image: Text<'static> = Span::styled("Converting image", no_img_style).into();
        let colored_text = match &self.state.img_state {
            ImgState::Idle(Some((_, text))) => text,
            ImgState::Idle(None) => &no_image,
            ImgState::Converting(_) => &converting_image,
        };

        let width: u16 = (area.height - 10) * 2;
        let height: u16 = area.height - 10;
        let area = Rect {
            width,
            height,
            x: (area.width - width) / 2,
            y: (area.height - height) / 2,
        };

        let padding = Padding::symmetric(
            2,
            if colored_text.height() > 1 {
                1
            } else {
                height / 2 - 3
            },
        );

        Paragraph::new(colored_text.clone())
            .centered()
            .block(block.padding(padding))
            .render(area, buf);
    }
}
