use std::io::Cursor;
use std::thread;
use std::time::Duration;

use crossbeam::channel::{bounded, TryRecvError};
use image::ImageOutputFormat;
use lib::{for_format, AudioRecorder, Format};
use nalgebra::UnitQuaternion;
use rosrust::{ros_err, ros_info, ros_warn};

mod msgs {
    pub use rosrust_msg::geometry_msgs::*;
    pub use rosrust_msg::sensor_msgs::*;
    pub use rosrust_msg::ssloc::*;
    pub use rosrust_msg::std_msgs::{ColorRGBA, Header};
    pub use rosrust_msg::visualization_msgs::*;
}

type Result<T = (), E = rosrust::error::Error> = std::result::Result<T, E>;

mod config;
use config::Config;
use wav::BitDepth;

fn main() -> Result {
    env_logger::init();

    rosrust::init("ssloc");

    let mut config_server = rosrust_dynamic_reconfigure::Server::<Config>::new(Config::init()?)?;

    let updating_config = config_server.get_config_updating();

    // TODO consider multiple consumers
    let (audio_channel_send, audio_channel_recv) = bounded(1);
    let audio_recorder = {
        let audio_channel_recv = audio_channel_recv.clone();
        let updating_config = updating_config.clone();
        thread::Builder::new()
            .name("audio recorder".to_owned())
            .spawn(move || -> Result {
                let audio_topic = rosrust::publish::<msgs::Audio>("~source_audio", 20)?;
                let mut config = updating_config.copy();
                'recorder: while rosrust::is_ok() {
                    for_format!(config.format, {
                        let mut recorder = match AudioRecorder::<FORMAT>::new(
                            config.device.name.clone(),
                            config.channels.into(),
                            config.rate.into(),
                            config.format,
                            config.localisation_frame,
                        ) {
                            Ok(recorder) => recorder,
                            Err(e) => {
                                ros_err!("error creating the audio recorder {e}");
                                thread::sleep(Duration::from_secs(1));
                                continue;
                            }
                        };

                        while rosrust::is_ok() {
                            let stamp = rosrust::now();
                            let header = msgs::Header {
                                stamp,
                                frame_id: "ssloc".to_string(),
                                ..Default::default()
                            };
                            let update = updating_config.read();
                            if update.channels != config.channels
                                || update.device != config.device
                                || update.rate != config.rate
                                || update.format != config.format
                                || update.localisation_frame != config.localisation_frame
                            {
                                config = update.clone();
                                continue 'recorder;
                            }
                            let audio = match recorder.record() {
                                Ok(audio) => audio,
                                Err(err) => {
                                    ros_err!("error recording audio {err}");
                                    continue 'recorder;
                                }
                            };
                            if let Err(err) = audio_topic.send(msgs::Audio {
                                header,
                                data: audio.wav(BitDepth::ThirtyTwoFloat),
                            }) {
                                ros_err!("error sending audio message {err}");
                            };
                            if audio_channel_send.is_full() {
                                match audio_channel_recv.try_recv() {
                                    Ok((stamp, _)) => {
                                        ros_warn!(
                                            "recording from {stamp} was dropped, ssloc operation \
                                             too slow"
                                        );
                                    }
                                    Err(TryRecvError::Empty) => { /* was emptied by consumer */ }
                                    Err(TryRecvError::Disconnected) => {
                                        ros_err!("channel disconnected, process must have exited");
                                        return Ok(());
                                    }
                                }
                            }
                            match audio_channel_send.send((stamp, audio)) {
                                Ok(_) => {}
                                Err(_) => {
                                    ros_err!("channel disconnected, process must have exited");
                                    return Ok(());
                                }
                            }
                        }
                    });
                }
                Ok(())
            })
            .expect("spawning audio thread should not panic")
    };

    let ssloc = thread::Builder::new()
        .name("ssloc".to_owned())
        .spawn(move || -> Result {
            let arrow_markers = rosrust::publish::<msgs::Marker>("~arrow_markers", 20)?;
            let unit_sphere_ssl = rosrust::publish::<msgs::UnitSslArray>("~unit_sphere_ssl", 20)?;
            let unit_sphere_points = rosrust::publish::<msgs::PointCloud2>("~unit_sphere_points", 20)?;
            let spectrums = rosrust::publish::<msgs::CompressedImage>("~spectrum", 20)?;

            let mut config = updating_config.copy();

            'mbss: while rosrust::is_ok() {
                let mbss = config
                    .mbss
                    .create(config.mics[..config.channels as usize].to_owned());
                while rosrust::is_ok() {
                    let max_sources = {
                        let update = updating_config.read();
                        if update.channels != config.channels
                            || update.mics != config.mics
                            || update.mbss != config.mbss
                        {
                            config = update.clone();
                            continue 'mbss;
                        }
                        update.max_sources.into()
                    };
                    let Ok((stamp, audio)) = audio_channel_recv.recv() else {
                    ros_err!("channel disconnected, process must have exited");
                    return Ok(());
                };
                    let header = msgs::Header {
                        stamp,
                        frame_id: "ssloc".to_string(),
                        ..Default::default()
                    };
                    if audio.channels() != config.channels as usize {
                        ros_info!("channels of recording missmatched, probably config was updated");
                        continue;
                    }
                    let spectrum = mbss.analyze_spectrum(&audio);
                    let mut data: Vec<u8> = Vec::new();
                    lib::spec_to_image(spectrum.view())
                        .write_to(&mut Cursor::new(&mut data), ImageOutputFormat::Png)
                        .unwrap();
                    if let Err(e) = spectrums.send(msgs::CompressedImage {
                        header: header.clone(),
                        format: "png".to_string(),
                        data,
                    }) {
                        ros_err!("error sending spectrum image {e}");
                    }

                    let sources = mbss.find_sources(spectrum.view(), max_sources);
                    for (idx, (az, el, _strength)) in sources.into_iter().enumerate() {
                        let rotation = UnitQuaternion::from_euler_angles(0., -el, az).coords;
                        if let Err(e) = arrow_markers.send(msgs::Marker {
                            header: header.clone(),
                            ns: "sslocate".to_string(),
                            id: idx as i32 + 1,
                            type_: msgs::Marker::ARROW as i32,
                            pose: msgs::Pose {
                                position: msgs::Point {
                                    x: 0.,
                                    y: 0.,
                                    z: 0.,
                                },
                                orientation: msgs::Quaternion {
                                    x: rotation.x,
                                    y: rotation.y,
                                    z: rotation.z,
                                    w: rotation.w,
                                },
                            },
                            color: msgs::ColorRGBA {
                                r: 1.,
                                a: 1.,
                                ..Default::default()
                            },
                            scale: msgs::Vector3 {
                                // x: (strength / 2000.).clamp(0.2, 2.),
                                x: 1.,
                                y: 0.2,
                                z: 0.2,
                            },
                            action: msgs::Marker::ADD as i32,
                            lifetime: rosrust::Duration::from_seconds(1),
                            ..Default::default()
                        }) {
                            ros_err!("error sending marker {e}");
                        };
                    }
                }
            }
            Ok(())
        })
        .expect("should be able to start ssloc process");

    // Create object that maintains 10Hz between sleep requests
    let rate = rosrust::rate(10.0);

    // Breaks when a shutdown signal is sent
    while rosrust::is_ok() {
        // // Create string message
        // let msg = rosrust_msg::std_msgs::String {
        //     data: format!("hello world from rosrust {}", count),
        // };

        // Log event
        // rosrust::ros_info!("Publishing: {:?}", msg);

        // Send string message to topic via publisher

        // if log_names {
        //     rosrust::ros_info!("Subscriber names: {:?}",
        // chatter_pub.subscriber_names()); }

        // Sleep to maintain 10Hz rate
        rate.sleep();
    }
    ssloc.join().expect("ssloc thread should not panic")?;
    audio_recorder
        .join()
        .expect("audio_recorder should not panic")?;
    Ok(())
}
