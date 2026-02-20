package main

import (
	"log"
	"time"
)

// AlertSeverity represents alert importance
type AlertSeverity string

const (
	SeverityInfo     AlertSeverity = "INFO"
	SeverityWarning  AlertSeverity = "WARNING"
	SeverityCritical AlertSeverity = "CRITICAL"
)

// Alert represents a system alert
type Alert struct {
	Severity  AlertSeverity
	Title     string
	Message   string
	Timestamp time.Time
}

// AlertChannel represents an alert destination
type AlertChannel string

const (
	ChannelPagerDuty AlertChannel = "pagerduty"
	ChannelTelegram  AlertChannel = "telegram"
	ChannelDiscord   AlertChannel = "discord"
)

// Alerter dispatches alerts to configured channels
type Alerter struct {
	channels  []AlertChannel
	history   []Alert
	rateLimit time.Duration
	lastAlert map[string]time.Time
}

// NewAlerter creates a new alerter
func NewAlerter(channels []AlertChannel) *Alerter {
	return &Alerter{
		channels:  channels,
		history:   make([]Alert, 0),
		rateLimit: 5 * time.Minute,
		lastAlert: make(map[string]time.Time),
	}
}

// Send dispatches an alert to all configured channels
func (a *Alerter) Send(severity AlertSeverity, title, message string) {
	// Rate limiting: don't send same title within rateLimit window
	if last, ok := a.lastAlert[title]; ok {
		if time.Since(last) < a.rateLimit {
			return
		}
	}

	alert := Alert{
		Severity:  severity,
		Title:     title,
		Message:   message,
		Timestamp: time.Now(),
	}

	a.history = append(a.history, alert)
	a.lastAlert[title] = time.Now()

	for _, ch := range a.channels {
		a.dispatch(ch, alert)
	}
}

func (a *Alerter) dispatch(channel AlertChannel, alert Alert) {
	// In production, this would call the actual API for each channel
	switch channel {
	case ChannelPagerDuty:
		log.Printf("[PagerDuty] [%s] %s: %s", alert.Severity, alert.Title, alert.Message)
	case ChannelTelegram:
		log.Printf("[Telegram] [%s] %s: %s", alert.Severity, alert.Title, alert.Message)
	case ChannelDiscord:
		log.Printf("[Discord] [%s] %s: %s", alert.Severity, alert.Title, alert.Message)
	}
}

// History returns recent alerts
func (a *Alerter) History() []Alert {
	return a.history
}
