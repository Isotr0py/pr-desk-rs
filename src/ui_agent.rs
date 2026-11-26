use egui::{Color32, RichText};

use pr_desk_rs::i18n::{tr, trf};

use crate::EguiApp;
use crate::state::*;

fn open_pr(app: &mut EguiApp, repo: &str, number: i64) {
    app.app.lock_recover().selected = Some((repo.to_string(), number));
    let _ = app.cmd_tx.send(Cmd::OpenDrawer {
        repo: repo.to_string(),
        number,
        refetch: true,
    });
}

fn decision_answer(
    app: &mut EguiApp,
    repo: &str,
    number: i64,
    decision: &AgentDecisionRow,
    answer: String,
    execute: bool,
) {
    app.agent_confirm = None;
    let _ = app.cmd_tx.send(Cmd::AnswerAgentDecision {
        repo: repo.to_string(),
        number,
        decision_id: decision.id,
        answer,
        execute,
    });
}

fn decision_ui(
    app: &mut EguiApp,
    ui: &mut egui::Ui,
    repo: &str,
    number: i64,
    decision: &AgentDecisionRow,
) {
    ui.group(|ui| {
        let lang = app.lang();
        ui.label(RichText::new(tr(&lang, "agent_decision")).strong());
        ui.label(&decision.title);
        if !decision.context.is_empty() {
            ui.weak(&decision.context);
        }
        let external = decision.kind != "clarification";
        ui.horizontal_wrapped(|ui| {
            for (id, label) in &decision.options {
                if ui.button(label).clicked() {
                    if external {
                        app.agent_confirm = Some((decision.id, id.clone()));
                    } else {
                        decision_answer(app, repo, number, decision, id.clone(), true);
                    }
                }
            }
        });
        if external {
            if let Some((decision_id, answer)) = app.agent_confirm.clone() {
                if decision_id == decision.id {
                    ui.colored_label(Color32::YELLOW, tr(&lang, "agent_confirm_external"));
                    ui.horizontal(|ui| {
                        if ui.button(tr(&lang, "agent_confirm_execute")).clicked() {
                            decision_answer(app, repo, number, decision, answer, true);
                        }
                        if ui.button(tr(&lang, "cancel")).clicked() {
                            let answer = app.agent_confirm.clone().unwrap_or_default();
                            decision_answer(app, repo, number, decision, answer.1, false);
                        }
                    });
                }
            }
            return;
        }
        let draft_id = egui::Id::unique(("agent-decision-draft", decision.id));
        let mut draft =
            ui.memory_mut(|memory| memory.data.get_temp::<String>(draft_id).unwrap_or_default());
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut draft)
                    .hint_text(tr(&lang, "agent_decision_free_text"))
                    .desired_width(280.0),
            );
            if response.changed() {
                ui.memory_mut(|memory| memory.data.insert_temp(draft_id, draft.clone()));
            }
            if ui
                .add_enabled(
                    !draft.trim().is_empty(),
                    egui::Button::new(tr(&lang, "agent_decision_send")),
                )
                .clicked()
            {
                decision_answer(app, repo, number, decision, draft.clone(), true);
                ui.memory_mut(|memory| memory.data.insert_temp(draft_id, String::new()));
            }
        });
    });
}

pub fn agent_review_section(app: &mut EguiApp, ui: &mut egui::Ui) {
    let Some(panel) = app.panel.as_ref() else {
        return;
    };
    let repo = panel.repo.clone();
    let number = panel.number;
    let agent = panel.agent_review.clone();
    let lang = app.lang();

    ui.group(|ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tr(&lang, "agent_review_title")).strong());
            if agent.has {
                ui.weak(&agent.status);
                if agent.stale {
                    ui.colored_label(Color32::YELLOW, tr(&lang, "agent_review_stale"));
                }
            }
            if !agent.has {
                if ui.button(tr(&lang, "agent_start")).clicked() {
                    let _ = app.cmd_tx.send(Cmd::StartAgentReview {
                        repo: repo.clone(),
                        number,
                    });
                }
            } else {
                let running = matches!(
                    agent.status.as_str(),
                    "queued" | "preparing" | "running" | "validating"
                );
                if running {
                    if ui.button(tr(&lang, "agent_cancel")).clicked() {
                        let _ = app.cmd_tx.send(Cmd::CancelAgentRun {
                            repo: repo.clone(),
                            number,
                        });
                    }
                } else {
                    let label = if agent.reviewed_head_sha.is_empty() {
                        tr(&lang, "agent_start")
                    } else {
                        tr(&lang, "agent_rereview")
                    };
                    if ui.button(label).clicked() {
                        let _ = app.cmd_tx.send(Cmd::RereviewAgent {
                            repo: repo.clone(),
                            number,
                        });
                    }
                }
                if matches!(agent.status.as_str(), "completed" | "waiting_decision")
                    && ui.button(tr(&lang, "agent_acknowledge")).clicked()
                {
                    let _ = app.cmd_tx.send(Cmd::AcknowledgeAgentReview {
                        repo: repo.clone(),
                        number,
                    });
                }
            }
        });

        if agent.has {
            if !agent.reviewed_head_sha.is_empty() {
                ui.weak(trf(
                    &lang,
                    "agent_reviewed_head",
                    &[&agent.reviewed_head_sha.chars().take(12).collect::<String>()],
                ));
            }
            if !agent.error.is_empty() {
                ui.colored_label(Color32::LIGHT_RED, &agent.error);
            }
            if !agent.verdict.is_empty() {
                ui.label(trf(&lang, "agent_verdict", &[&agent.verdict]));
            }
            if !agent.summary.is_empty() {
                egui_commonmark::CommonMarkViewer::new().show(
                    ui,
                    &mut app.md_cache,
                    &agent.summary,
                );
            }
            if !agent.validation.is_empty() {
                ui.label(RichText::new(tr(&lang, "agent_validation")).strong());
                ui.label(&agent.validation);
            }
            for decision in &agent.decisions {
                decision_ui(app, ui, &repo, number, decision);
            }
            if !agent.findings.is_empty() {
                ui.separator();
                ui.label(RichText::new(tr(&lang, "agent_findings")).strong());
                for finding in &agent.findings {
                    ui.group(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.colored_label(
                                crate::ui_detail::severity_color(
                                    &finding.severity,
                                    ui.visuals().dark_mode,
                                ),
                                &finding.severity,
                            );
                            ui.monospace(&finding.location);
                            ui.weak(&finding.status);
                            if finding.human_status != "undecided" {
                                ui.weak(&finding.human_status);
                            }
                        });
                        ui.label(&finding.title);
                        if !finding.evidence.is_empty() {
                            ui.weak(&finding.evidence);
                        }
                        if !finding.suggestion.is_empty() {
                            ui.label(&finding.suggestion);
                        }
                        if !finding.resolution_note.is_empty() {
                            ui.weak(&finding.resolution_note);
                        }
                        ui.horizontal_wrapped(|ui| {
                            for (status, key) in [
                                ("accepted", "finding_accept"),
                                ("dismissed", "finding_dismiss"),
                                ("undecided", "finding_reopen"),
                            ] {
                                if finding.human_status != status
                                    && ui.small_button(tr(&lang, key)).clicked()
                                {
                                    let _ = app.cmd_tx.send(Cmd::SetFindingHumanStatus {
                                        repo: repo.clone(),
                                        number,
                                        finding_id: finding.id,
                                        status: status.to_string(),
                                    });
                                }
                            }
                        });
                    });
                }
            }
        }
    });
}

pub fn needs_you_view(app: &mut EguiApp, ui: &mut egui::Ui) {
    let lang = app.lang();
    ui.heading(tr(&lang, "needs_you_title"));
    if app.app.lock_recover().needs_you_loading {
        ui.spinner();
        return;
    }
    let rows = app.app.lock_recover().needs_you.clone();
    if rows.is_empty() {
        ui.weak(tr(&lang, "needs_you_empty"));
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for row in rows {
            ui.group(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("{} #{}", row.repo, row.number)).strong());
                    ui.weak(&row.kind);
                    if ui.button(tr(&lang, "needs_you_open")).clicked() {
                        open_pr(app, &row.repo, row.number);
                    }
                    match row.kind.as_str() {
                        "stale" | "ci_failure" => {
                            if ui.button(tr(&lang, "agent_rereview")).clicked() {
                                let _ = app.cmd_tx.send(Cmd::RereviewAgent {
                                    repo: row.repo.clone(),
                                    number: row.number,
                                });
                            }
                        }
                        "failed" => {
                            if ui.button(tr(&lang, "agent_retry")).clicked() {
                                let _ = app.cmd_tx.send(Cmd::StartAgentReview {
                                    repo: row.repo.clone(),
                                    number: row.number,
                                });
                            }
                        }
                        "completed" => {
                            if ui.button(tr(&lang, "agent_acknowledge")).clicked() {
                                let _ = app.cmd_tx.send(Cmd::AcknowledgeAgentReview {
                                    repo: row.repo.clone(),
                                    number: row.number,
                                });
                            }
                        }
                        _ => {}
                    }
                });
                ui.label(&row.title);
                if !row.detail.is_empty() {
                    ui.weak(&row.detail);
                }
                if row.kind == "decision" {
                    let decision = AgentDecisionRow {
                        id: row.decision_id,
                        run_id: row.run_id,
                        kind: row.decision_kind.clone(),
                        title: row.title.clone(),
                        context: row.detail.clone(),
                        options: row.options.clone(),
                        ..Default::default()
                    };
                    decision_ui(app, ui, &row.repo, row.number, &decision);
                }
            });
        }
    });
}
