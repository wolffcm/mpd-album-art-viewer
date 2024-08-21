use ansi_to_tui::IntoText;
use clap::Parser;
use core::str::FromStr;
use image::{io::Reader as ImageReader, DynamicImage};
use img_to_ascii::{
    convert::{self, get_conversion_algorithm, get_converter},
    font::Font,
    image::LumaImage,
};
use log::{debug, info, warn};
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
use std::{error::Error, path::Path, thread::JoinHandle};
use std::{
    io::{stdout, Cursor},
    net::ToSocketAddrs,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long, value_name = "HOST", default_value = "localhost")]
    host: String,
    #[arg(long, value_name = "PORT", default_value_t = 6600)]
    port: u16,
    #[arg(long, value_name = "LEVEL", default_value = "WARN")]
    log_level_filter: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    match std::env::var_os("XDG_STATE_HOME") {
        None => (),
        Some(xdg_state_home) => {
            let mut log_path = PathBuf::from(xdg_state_home);
            log_path.push(env!("CARGO_PKG_NAME"));
            log_path.push("log");
            let log_level_filter: log::LevelFilter =
                log::LevelFilter::from_str(&args.log_level_filter)?;
            match simple_logging::log_to_file(&log_path, log_level_filter) {
                Ok(()) => Ok(()),
                Err(err) => Err(format!(
                    "error logging to {}: {:?}",
                    log_path.display(),
                    err
                )),
            }?;
            info!(target: "default", "starting logging");
        }
    }

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
        matches!(self, ImgState::Idle(_))
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
    viewport_area: Rect,
    current_song: Option<Song>,
    mpd_status: MpdStatus,
    img_state: ImgState,
}

struct App {
    client: MpdClient,
    font: Font,
    font_aspect: f64,
    state: State,
    last_update_time: Option<Instant>,
    exit: bool,
}

const HORIZ_BORDER_WIDTH: usize = 1;
const VERT_BORDER_WIDTH: usize = 1;
const HORIZ_PADDING: usize = 2;
const VERT_PADDING: usize = 1;
const HORIZ_VIEWPORT_GAP: usize = 6;
const VERT_VIEWPORT_GAP: usize = 3;

impl App {
    const UPDATE_PERIOD: Duration = Duration::from_secs(1);
    const ALPHABET: &'static str = include_str!("../alphabets/alphabet.txt");
    const BDF_FILE: &'static str = include_str!("../fonts/bitocra-13.bdf");

    pub fn create(host_port: &str) -> Result<Self> {
        let mut addrs_iter = host_port.to_socket_addrs()?;
        let addr = match addrs_iter.next() {
            None => return Err("could not resolve host".into()),
            Some(addr) => addr,
        };

        let client = MpdClient::connect(addr)?;
        let alphabet = Self::ALPHABET.chars().collect::<Vec<char>>();
        let font = Font::from_bdf_stream(Self::BDF_FILE.as_bytes(), &alphabet);
        let font_aspect = font.width as f64 / font.height as f64;
        info!(
            "font has width {} and height {}; aspect: {}",
            font.width, font.height, font_aspect
        );
        Ok(App {
            font,
            font_aspect,
            client,
            state: State::default(),
            last_update_time: None,
            exit: false,
        })
    }

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        self.state.viewport_area = terminal.get_frame().size();

        self.update_app_state()?;
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
        if let KeyCode::Char('q') = key_event.code {
            self.exit();
        }
    }

    fn songs_in_same_dir(song0: &Song, song1: &Song) -> bool {
        let dir0 = Path::new(&song0.file).parent();
        let dir1 = Path::new(&song1.file).parent();
        debug!("songs_in_same_dir: {:?}, {:?}", dir0, dir1);
        dir0 == dir1
    }

    fn update_app_state(&mut self) -> Result<()> {
        self.state.mpd_status = self.client.status()?;
        let old_song = self.state.current_song.take();
        let new_song = self.client.currentsong()?;
        let (song_changed, album_art_changed) = match (&old_song, &new_song) {
            (None, None) => (false, false),
            (Some(song0), Some(song1)) if song0 == song1 => (false, false),
            (Some(song0), Some(song1)) => (true, !Self::songs_in_same_dir(song0, song1)),
            _ => (true, true),
        };

        self.state.current_song = new_song;

        if song_changed {
            debug!("song changed: {song_changed}; album_art_changed: {album_art_changed}");
        }

        if song_changed && self.state.img_state.is_idle() && album_art_changed {
            // enter converting state
            let art: Option<Vec<u8>> =
                self.state
                    .current_song
                    .as_ref()
                    .and_then(|song| -> Option<Vec<u8>> {
                        self.client
                            .albumart(song)
                            .inspect_err(|err| {
                                warn!("error fetching album art for \"{}\": {:?}", song.file, err)
                            })
                            .ok()
                    });

            let font = self.font.clone();
            let font_aspect = self.font_aspect;
            let area = self.state.viewport_area;
            let jh = std::thread::spawn(move || -> Option<(DynamicImage, Text<'static>)> {
                let art = match art {
                    None => return None,
                    Some(art) => art,
                };

                let dyn_img = ImageReader::new(Cursor::new(art))
                    .with_guessed_format()
                    .inspect_err(|err| warn!("error guessing image format: {:?}", err))
                    .ok()?
                    .decode()
                    .inspect_err(|err| warn!("error decoding image: {:?}", err))
                    .ok()?;
                let viewable_width = area.width as usize
                    - (HORIZ_VIEWPORT_GAP + HORIZ_BORDER_WIDTH + HORIZ_PADDING) * 2;
                let viewable_height = area.height as usize
                    - (VERT_VIEWPORT_GAP + VERT_BORDER_WIDTH + VERT_PADDING) * 2;
                let viewport_aspect = viewable_width as f64 * font_aspect / viewable_height as f64;
                let image_aspect = dyn_img.width() as f64 / dyn_img.height() as f64;
                info!("viewport: {}; aspect: {}", area, viewport_aspect);
                info!(
                    "image: {} x {}; aspect: {}",
                    dyn_img.width(),
                    dyn_img.height(),
                    image_aspect
                );
                let width = if image_aspect > viewport_aspect {
                    // Image is wide compared to the viewport, so width will be the determining
                    // factor when scaling.
                    area.width as usize
                        - (HORIZ_VIEWPORT_GAP + HORIZ_BORDER_WIDTH + HORIZ_PADDING) * 2
                } else {
                    // Image is tall compared to the viewport, so height will be the determining
                    // factor when scaling.
                    //
                    // (VERT_VIEWPORT_GAP + VERT_BORDER_WIDTH + VERT_PADDING) * 2 + ascii_img_width * font_aspect / img_aspect ==
                    //   viewport_height
                    //
                    // ascii_img_height == ascii_img_width * font_aspect / img_aspect
                    // Solving for width:
                    //
                    // width = (viewport_height - ((VERT_VIEWPORT_GAP + VERT_BORDER_WIDTH + VERT_PADDING) * 2)) / font_aspect;
                    ((area.height as usize
                        - ((VERT_VIEWPORT_GAP + VERT_BORDER_WIDTH + VERT_PADDING) * 2))
                        as f64
                        * image_aspect
                        / font_aspect) as usize
                };
                info!("scaled ascii image width: {}", width);
                let rows = convert::img_to_char_rows(
                    &font,
                    &LumaImage::from(&dyn_img),
                    get_converter("direction-and-intensity"),
                    Some(width),
                    0.0,
                    &get_conversion_algorithm("edge-augmented"),
                );
                {
                    let converted_width = rows[0].len();
                    let converted_height = rows.len();
                    let aspect0 = converted_width as f64 / converted_height as f64;
                    let aspect1 = converted_height as f64 / converted_width as f64;

                    info!(
                        "converted image is {} x {}; aspect {} or {}",
                        rows[0].len(),
                        rows.len(),
                        aspect0,
                        aspect1
                    );
                }

                let text = convert::char_rows_to_terminal_color_string(&rows, &dyn_img)
                    .into_text()
                    .inspect_err(|err| warn!("error converting ANSI to `Text`: {:?}", err))
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

    fn create_paragraph(&self, buf: &mut Buffer, viewport_area: Rect, block: Block, text: &Text) {
        let (width, height, vert_padding) = if text.height() > 1 {
            // This is an image
            let width = (text.width() + (HORIZ_BORDER_WIDTH + HORIZ_PADDING) * 2) as u16;
            let height = (text.height() + (VERT_BORDER_WIDTH + VERT_PADDING) * 2) as u16;
            (width, height, VERT_PADDING)
        } else {
            // This is a message
            let viewable_width = viewport_area.width as usize - HORIZ_VIEWPORT_GAP * 2;
            let viewable_height = viewport_area.height as usize - VERT_VIEWPORT_GAP * 2;
            let viewport_aspect = viewable_width as f64 * self.font_aspect / viewable_height as f64;
            if viewport_aspect < 1.0 {
                // Taller than it is wide; use width to form a square.
                let width = viewable_width as u16;
                let height = (width as f64 * self.font_aspect) as u16;
                (width, height, viewable_height / 2 - VERT_BORDER_WIDTH - 2)
            } else {
                // Wider than it is tall; Use height to form a square
                let height = viewable_height as u16;
                let width = (height as f64 / self.font_aspect) as u16;
                (width, height, viewable_height / 2 - VERT_BORDER_WIDTH - 2)
            }
        };
        let area = Rect {
            width,
            height,
            x: (viewport_area.width - width) / 2,
            y: (viewport_area.height - height) / 2,
        };

        let padding = Padding::symmetric(HORIZ_PADDING as u16, vert_padding as u16);

        Paragraph::new(text.clone())
            .centered()
            .block(block.padding(padding))
            .render(area, buf);
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

        self.create_paragraph(buf, area, block, &colored_text);
    }
}
